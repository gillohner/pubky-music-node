//! Your Audiobookshelf as a source (STEWARD-COMPAT-HA-MA.md §6.3): the audiobooks in its book
//! libraries, through its documented API (api.audiobookshelf.org).
//!
//! - **Signing in:** an API key (Audiobookshelf 2.26+, Settings › API Keys), or a user and
//!   password once: for an admin the node makes itself a non-expiring API key and keeps only
//!   that (Audiobookshelf's session tokens expire within the hour); other users are asked for a
//!   key, since only admins can make one.
//! - **Playing:** each audio file of a book queues as `audiobookshelf:file:<item>/<ino>` and plays
//!   the original file; browser streams and covers are relayed, so the key never reaches the page.
//! - **Resume, shared with your phone:** `POST …/play` starts where Audiobookshelf says you left
//!   off, and while a zone plays a book the node reports the position back every few seconds.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use pm_zones::{Insert, QueueItem, ZoneId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const SOURCE_ID: &str = "audiobookshelf";
pub const TRACK_URI_PREFIX: &str = "audiobookshelf:file:";
const SETTING: &str = "audiobookshelf.server";
const KEY_NAME: &str = "Pubky Music node";
const PROGRESS_EVERY: Duration = Duration::from_secs(10);

/// The connected server. `Debug` never shows the key.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub url: String,
    pub api_key: String,
    /// The key's id when the node made the key itself (so signing out deletes it).
    #[serde(default)]
    pub key_id: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("api_key", &"<set>")
            .finish()
    }
}

/// One audio file of a book: where it starts in the book.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub ino: String,
    pub start_ms: u64,
    pub duration_ms: u64,
}

/// A book as the node needs it to report progress.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Book {
    pub duration_ms: u64,
    pub parts: Vec<Part>,
}

#[derive(Clone, Debug, Default)]
pub struct Abs {
    pub server: Option<Server>,
    /// Books seen recently, by item id.
    pub books: HashMap<String, Book>,
    /// The last position reported per item (ms), so an unchanged one is not sent again.
    pub reported: HashMap<String, u64>,
}

pub type Shared = Arc<RwLock<Abs>>;

pub fn load(lib: &pm_library::Library) -> Abs {
    Abs {
        server: lib
            .get_setting(SETTING)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok()),
        ..Default::default()
    }
}

pub fn track_uri(item: &str, ino: &str) -> String {
    format!("{TRACK_URI_PREFIX}{item}/{ino}")
}

/// `<item>/<ino>` from a track URI's tail.
fn split_track(rest: &str) -> Option<(&str, &str)> {
    let (item, ino) = rest.split_once('/')?;
    (valid_id(item).is_ok() && valid_id(ino).is_ok()).then_some((item, ino))
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("Audiobookshelf: {e}"))
}

/// Item ids are UUIDs (`li_…` on old servers), file inodes are numbers.
fn valid_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::bad_request("bad Audiobookshelf id"));
    }
    Ok(id)
}

fn server(s: &NodeState) -> Result<Server, ApiError> {
    s.audiobookshelf
        .read()
        .expect("audiobookshelf")
        .server
        .clone()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_FAILED,
                "connect your Audiobookshelf first (Sources › Audiobookshelf)",
            )
        })
}

pub(crate) fn connected(s: &NodeState) -> bool {
    s.audiobookshelf.read().expect("audiobookshelf").server.is_some()
}

fn base(srv: &Server) -> &str {
    srv.url.trim_end_matches('/')
}

async fn send(rb: reqwest::RequestBuilder, what: &str) -> Result<Value, ApiError> {
    let resp = rb.send().await.map_err(|e| upstream(e.without_url()))?;
    match resp.status() {
        st if st.is_success() => {
            let body = resp.bytes().await.map_err(|e| upstream(e.without_url()))?;
            Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
        }
        StatusCode::UNAUTHORIZED => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Audiobookshelf no longer accepts this key: connect again (Sources › Audiobookshelf)",
        )),
        StatusCode::NOT_FOUND => Err(ApiError::not_found("Audiobookshelf has no such item")),
        st => Err(upstream(format!("{what} answered {st}"))),
    }
}

async fn get(s: &NodeState, srv: &Server, path: &str, query: &[(&str, &str)]) -> Result<Value, ApiError> {
    send(
        s.http_client()
            .get(format!("{}{path}", base(srv)))
            .bearer_auth(&srv.api_key)
            .query(query),
        path,
    )
    .await
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn secs_ms(v: &Value) -> Option<u64> {
    v.as_f64()
        .filter(|d| *d >= 0.0)
        .map(|d| (d * 1000.0).round() as u64)
}

fn cover_path(item: &str) -> String {
    format!("/v1/sources/audiobookshelf/cover/{item}")
}

fn image_of(it: &Value) -> Option<String> {
    (it["media"]["coverPath"].is_string())
        .then(|| str_of(it, "id").map(|id| cover_path(&id)))
        .flatten()
}

fn author_of(it: &Value) -> Option<String> {
    str_of(&it["media"]["metadata"], "authorName")
}

fn year_of(it: &Value) -> Option<i32> {
    it["media"]["metadata"]["publishedYear"]
        .as_str()
        .and_then(|y| y.get(..4)?.parse().ok())
}

pub(crate) fn hit(it: &Value) -> Option<SourceHit> {
    if it["mediaType"].as_str()? != "book" {
        return None;
    }
    let id = str_of(it, "id")?;
    Some(SourceHit {
        id: format!("book:{id}"),
        title: str_of(&it["media"]["metadata"], "title")?,
        creator: author_of(it),
        year: year_of(it),
        downloads: None,
        image: image_of(it),
        kind: "album".into(),
        duration_ms: secs_ms(&it["media"]["duration"]),
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: String::new(),
    })
}

/// The book's files in order, each with where it starts (Audiobookshelf's `media.tracks`).
pub(crate) fn book_of(it: &Value) -> Book {
    let files: HashMap<u64, String> = it["media"]["audioFiles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| Some((f["index"].as_u64()?, str_of(f, "ino")?)))
                .collect()
        })
        .unwrap_or_default();
    let parts = it["media"]["tracks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some(Part {
                        ino: files.get(&t["index"].as_u64()?)?.clone(),
                        start_ms: secs_ms(&t["startOffset"])?,
                        duration_ms: secs_ms(&t["duration"]).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Book {
        duration_ms: secs_ms(&it["media"]["duration"]).unwrap_or(0),
        parts,
    }
}

/// A part's title: the chapter that starts with it, else "Part n".
fn part_title(it: &Value, p: &Part, n: usize) -> String {
    it["media"]["chapters"]
        .as_array()
        .and_then(|cs| {
            cs.iter()
                .find(|c| secs_ms(&c["start"]).is_some_and(|s| s.abs_diff(p.start_ms) < 500))
        })
        .and_then(|c| str_of(c, "title"))
        .unwrap_or_else(|| format!("Part {n}"))
}

fn format_of(it: &Value, ino: &str) -> String {
    let f = it["media"]["audioFiles"]
        .as_array()
        .and_then(|a| a.iter().find(|f| f["ino"].as_str() == Some(ino)));
    match f.map(|f| (f["codec"].as_str(), f["bitRate"].as_u64())) {
        Some((Some(c), Some(b))) => format!("{} {} kbps (Audiobookshelf)", c.to_uppercase(), b / 1000),
        Some((Some(c), None)) => format!("{} (Audiobookshelf)", c.to_uppercase()),
        _ => "Audiobookshelf".into(),
    }
}

async fn fetch_item(s: &NodeState, srv: &Server, id: &str) -> Result<Value, ApiError> {
    let it = get(s, srv, &format!("/api/items/{id}"), &[("expanded", "1")]).await?;
    s.audiobookshelf
        .write()
        .expect("audiobookshelf")
        .books
        .insert(id.to_string(), book_of(&it));
    Ok(it)
}

pub struct Audiobookshelf;

#[async_trait::async_trait]
impl StreamingService for Audiobookshelf {
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
        let srv = s.audiobookshelf.read().expect("audiobookshelf").server.clone();
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Audiobookshelf".into(),
            description: match &srv {
                Some(v) => format!("Your audiobooks at {}", v.url),
                None => "Your Audiobookshelf server's audiobooks".into(),
            },
            status: if srv.is_some() { "ready" } else { "needs_config" }.into(),
        }
    }

    fn unified(&self, s: &NodeState) -> bool {
        connected(s)
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let srv = server(s)?;
        let libs = get(s, &srv, "/api/libraries", &[]).await?;
        let n = limit.to_string();
        let mut out = Vec::new();
        for lib in libs["libraries"].as_array().cloned().unwrap_or_default() {
            if lib["mediaType"].as_str() != Some("book") {
                continue;
            }
            let Some(id) = str_of(&lib, "id") else { continue };
            let id = valid_id(&id)?.to_string();
            let r = get(
                s,
                &srv,
                &format!("/api/libraries/{id}/search"),
                &[("q", term), ("limit", &n)],
            )
            .await?;
            out.extend(
                r["book"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| hit(&b["libraryItem"]))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            );
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let srv = server(s)?;
        let raw = id.strip_prefix("book:").unwrap_or(id);
        let raw = valid_id(raw)?;
        let it = fetch_item(s, &srv, raw).await?;
        let book = book_of(&it);
        let meta = &it["media"]["metadata"];
        Ok(SourceItem {
            source: SOURCE_ID.into(),
            id: format!("book:{raw}"),
            title: str_of(meta, "title").unwrap_or_else(|| "Untitled".into()),
            creator: author_of(&it),
            year: year_of(&it),
            description: str_of(meta, "description"),
            licence: None,
            image: image_of(&it),
            url: String::new(),
            tracks: book
                .parts
                .iter()
                .enumerate()
                .map(|(i, p)| SourceTrack {
                    title: part_title(&it, p, i + 1),
                    track_no: Some(i as u32 + 1),
                    duration_ms: Some(p.duration_ms),
                    format: format_of(&it, &p.ino),
                    url: track_uri(raw, &p.ino),
                    artist: author_of(&it),
                })
                .collect(),
            library_album_id: None,
        })
    }
}

// ---- playback ----

/// Open `audiobookshelf:file:<item>/<ino>` for a zone: the original file, the key kept in the node.
pub(crate) fn open_track_blocking(
    shared: &Shared,
    rest: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: format!("{TRACK_URI_PREFIX}{rest}"),
        reason,
    };
    let (item, ino) = split_track(rest).ok_or_else(|| fail("bad Audiobookshelf file".into()))?;
    let srv = shared
        .read()
        .map_err(|_| fail("server settings".into()))?
        .server
        .clone()
        .ok_or_else(|| fail("no Audiobookshelf is connected".into()))?;
    // The HTTP reader takes a URL only: the key goes in Audiobookshelf's `token` query here,
    // kept inside the node.
    let url = format!("{}/api/items/{item}/file/{ino}?token={}", base(&srv), srv.api_key);
    crate::sources::open_http_stream(&url).map_err(|e| match e {
        pm_zones::SourceError::Open { reason, .. } => fail(reason.replace(&srv.api_key, "<key>")),
        other => other,
    })
}

async fn relay(
    s: &NodeState,
    url: String,
    fallback_type: &str,
) -> Result<axum::response::Response, ApiError> {
    let srv = server(s)?;
    let resp = s
        .http_client()
        .get(url)
        .bearer_auth(&srv.api_key)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(if resp.status() == StatusCode::NOT_FOUND {
            ApiError::not_found("Audiobookshelf has no such item")
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

/// `GET /v1/sources/audiobookshelf/stream/{item}/{ino}`: the original file for browsers.
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    Path((item, ino)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiError> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let (item, ino) = (valid_id(&item)?.to_string(), valid_id(&ino)?.to_string());
    let srv = server(&s)?;
    relay(
        &s,
        format!("{}/api/items/{item}/file/{ino}", base(&srv)),
        "audio/mpeg",
    )
    .await
}

/// `GET /v1/sources/audiobookshelf/cover/{item}`.
pub(crate) async fn cover(
    State(s): State<Arc<NodeState>>,
    Path(item): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let item = valid_id(&item)?.to_string();
    let srv = server(&s)?;
    relay(&s, format!("{}/api/items/{item}/cover", base(&srv)), "image/jpeg").await
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlayBook {
    pub zone: String,
    /// `book:<id>` or the bare item id.
    pub item: String,
    /// Start from the beginning even when Audiobookshelf has a position.
    #[serde(default)]
    pub restart: bool,
}

/// Where to start: the part holding `at_ms`, and the offset into it.
pub(crate) fn locate(book: &Book, at_ms: u64) -> (usize, u64) {
    book.parts
        .iter()
        .enumerate()
        .rev()
        .find(|(_, p)| p.start_ms <= at_ms)
        .map(|(i, p)| (i, at_ms - p.start_ms))
        .unwrap_or((0, 0))
}

/// `POST /v1/sources/audiobookshelf/play {zone, item, restart?}`: the whole book into the zone,
/// from where Audiobookshelf says you left off.
pub(crate) async fn play(State(s): State<Arc<NodeState>>, Json(b): Json<PlayBook>) -> ApiResult<PlayStarted> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let zone = s
        .zones
        .get(&ZoneId::new(&b.zone))
        .ok_or_else(|| ApiError::not_found(format!("no zone {}", b.zone)))?;
    let srv = server(&s)?;
    let id = valid_id(b.item.strip_prefix("book:").unwrap_or(&b.item))?.to_string();
    let it = fetch_item(&s, &srv, &id).await?;
    let book = book_of(&it);
    if book.parts.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "this item has no audio",
        ));
    }
    let resume = if b.restart {
        0
    } else {
        match get(&s, &srv, &format!("/api/me/progress/{id}"), &[]).await {
            Ok(p) if !p["isFinished"].as_bool().unwrap_or(false) => secs_ms(&p["currentTime"]).unwrap_or(0),
            Ok(_) => 0,
            Err(e) if e.status == StatusCode::NOT_FOUND => 0,
            Err(e) => return Err(e),
        }
    };
    let (index, offset) = locate(&book, resume);
    let title = str_of(&it["media"]["metadata"], "title");
    let items = book
        .parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut q = QueueItem::new(track_uri(&id, &p.ino));
            q.title = Some(part_title(&it, p, i + 1));
            q.artist = author_of(&it);
            q.album = title.clone();
            q.duration_ms = Some(p.duration_ms);
            q.image = image_of(&it);
            q
        })
        .collect();
    zone.stop();
    zone.clear();
    let ids = zone.enqueue(items, Insert::End);
    zone.play_index(index);
    if offset > 5_000 {
        zone.seek_ms(offset);
    }
    // What is already on the server needs no report
    s.audiobookshelf
        .write()
        .expect("audiobookshelf")
        .reported
        .insert(id, resume);
    Ok(Json(PlayStarted {
        ids,
        index,
        position_ms: resume,
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayStarted {
    pub ids: Vec<u64>,
    /// The part playing first, and the book position it starts from.
    pub index: usize,
    pub position_ms: u64,
}

/// `GET /v1/sources/audiobookshelf/continue`: the books you are in the middle of.
pub(crate) async fn in_progress(State(s): State<Arc<NodeState>>) -> ApiResult<SourceResults> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let srv = server(&s)?;
    let v = get(&s, &srv, "/api/me/items-in-progress", &[]).await?;
    Ok(Json(SourceResults {
        source: SOURCE_ID.into(),
        items: v["libraryItems"]
            .as_array()
            .map(|a| a.iter().filter_map(hit).collect())
            .unwrap_or_default(),
    }))
}

/// Report the position of books playing on any zone to Audiobookshelf, every few seconds.
pub fn spawn_background(s: &Arc<NodeState>) {
    let weak = Arc::downgrade(s);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { return };
            if connected(&s) && s.source_enabled(SOURCE_ID) {
                report_positions(&s).await;
            }
        }
    });
}

/// The book position of a zone playing part `ino` at `pos_ms`.
pub(crate) fn book_position(book: &Book, ino: &str, pos_ms: u64) -> Option<u64> {
    book.parts
        .iter()
        .find(|p| p.ino == ino)
        .map(|p| p.start_ms + pos_ms)
}

async fn report_positions(s: &NodeState) {
    let playing: Vec<(String, String, u64)> = s
        .zones
        .list()
        .into_iter()
        .filter_map(|z| {
            let np = z.now_playing?;
            let (item, ino) = split_track(np.item.uri.strip_prefix(TRACK_URI_PREFIX)?)?;
            Some((item.to_string(), ino.to_string(), np.position_ms))
        })
        .collect();
    let Ok(srv) = server(s) else { return };
    for (item, ino, pos) in playing {
        let cached = s
            .audiobookshelf
            .read()
            .expect("audiobookshelf")
            .books
            .get(&item)
            .cloned();
        let book = match cached {
            Some(b) => b,
            None => match fetch_item(s, &srv, &item).await {
                Ok(it) => book_of(&it),
                Err(_) => continue,
            },
        };
        let Some(at) = book_position(&book, &ino, pos) else {
            continue;
        };
        let last = s
            .audiobookshelf
            .read()
            .expect("audiobookshelf")
            .reported
            .get(&item)
            .copied();
        if last.is_some_and(|l| l.abs_diff(at) < 2_000) {
            continue;
        }
        let dur = book.duration_ms.max(at);
        // `isFinished` only when true: Audiobookshelf ignores `progress` in an update that has
        // it, and marks a book finished by itself near the end.
        let mut body = serde_json::json!({
            "currentTime": at as f64 / 1000.0,
            "duration": dur as f64 / 1000.0,
            "progress": if dur > 0 { at as f64 / dur as f64 } else { 0.0 },
        });
        if dur > 0 && at + 2_000 >= dur {
            body["isFinished"] = true.into();
        }
        let rb = s
            .http_client()
            .patch(format!("{}/api/me/progress/{item}", base(&srv)))
            .bearer_auth(&srv.api_key)
            .json(&body);
        match send(rb, "progress").await {
            Ok(_) => {
                s.audiobookshelf
                    .write()
                    .expect("audiobookshelf")
                    .reported
                    .insert(item, at);
            }
            Err(e) => tracing::debug!("Audiobookshelf progress not saved: {}", e.message),
        }
    }
}

// ---- connecting ----

/// The connected Audiobookshelf as the UI sees it: never the key.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AbsView {
    pub connected: bool,
    pub url: Option<String>,
    pub user: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
pub struct AbsConnect {
    pub url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

fn view(srv: Option<&Server>) -> AbsView {
    AbsView {
        connected: srv.is_some(),
        url: srv.map(|v| v.url.clone()),
        user: srv.and_then(|v| v.user.clone()),
        version: srv.and_then(|v| v.version.clone()),
    }
}

/// `GET /v1/sources/audiobookshelf/server`: never the key.
pub(crate) async fn get_server(State(s): State<Arc<NodeState>>) -> ApiResult<AbsView> {
    Ok(Json(view(
        s.audiobookshelf.read().expect("audiobookshelf").server.as_ref(),
    )))
}

/// `PUT /v1/sources/audiobookshelf/server {url, api_key}` or `{url, user, password}`.
pub(crate) async fn connect(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<AbsConnect>,
) -> ApiResult<AbsView> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let url = body.url.trim().trim_end_matches('/').to_string();
    let parsed = url::Url::parse(&url)
        .map_err(|_| ApiError::bad_request("the server address must be an http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("the server address must be an http(s) URL"));
    }
    let status: Value = s
        .http_client()
        .get(format!("{url}/status"))
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?
        .json()
        .await
        .ok()
        .filter(|v: &Value| v["app"].as_str() == Some("audiobookshelf"))
        .ok_or_else(|| upstream("this address does not answer like an Audiobookshelf server"))?;
    let version = str_of(&status, "serverVersion");
    let (api_key, key_id) = if !body.api_key.trim().is_empty() {
        (body.api_key.trim().to_string(), None)
    } else if !body.user.trim().is_empty() {
        let (key, id) = key_from_password(&s, &url, body.user.trim(), &body.password).await?;
        (key, Some(id))
    } else {
        return Err(ApiError::bad_request(
            "an API key, or a user and password, is required",
        ));
    };
    let mut srv = Server {
        url,
        api_key,
        key_id,
        user: None,
        version,
    };
    let me = get(&s, &srv, "/api/me", &[]).await.map_err(|e| {
        if e.status == StatusCode::PRECONDITION_FAILED {
            ApiError::new(StatusCode::UNAUTHORIZED, "Audiobookshelf refused this API key")
        } else {
            e
        }
    })?;
    srv.user = str_of(&me, "username");
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(&srv).expect("serializes"))?;
    tracing::info!(url = %srv.url, user = ?srv.user, "Audiobookshelf connected");
    let v = view(Some(&srv));
    s.audiobookshelf.write().expect("audiobookshelf").server = Some(srv);
    Ok(Json(v))
}

/// Signs in once and, for an admin, makes the node its own API key; the session is then ended.
async fn key_from_password(
    s: &NodeState,
    url: &str,
    user: &str,
    password: &str,
) -> Result<(String, String), ApiError> {
    let resp = s
        .http_client()
        .post(format!("{url}/login"))
        .header("x-return-tokens", "true")
        .json(&serde_json::json!({"username": user, "password": password}))
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "Audiobookshelf refused this user name or password",
        ));
    }
    if !resp.status().is_success() {
        return Err(upstream(format!("sign-in answered {}", resp.status())));
    }
    let v: Value = resp.json().await.map_err(|e| upstream(e.without_url()))?;
    let u = &v["user"];
    let token = str_of(u, "accessToken")
        .or_else(|| str_of(u, "token"))
        .ok_or_else(|| upstream("no token in the answer"))?;
    let refresh = str_of(u, "refreshToken");
    let end_session = || async {
        let mut rb = s.http_client().post(format!("{url}/logout")).bearer_auth(&token);
        if let Some(r) = &refresh {
            rb = rb.header("x-refresh-token", r);
        }
        let _ = rb.send().await;
    };
    if !matches!(u["type"].as_str(), Some("root" | "admin")) {
        end_session().await;
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only an Audiobookshelf admin can make API keys: ask yours for one (Settings › API Keys) and paste it here",
        ));
    }
    let user_id = str_of(u, "id").ok_or_else(|| upstream("no user id in the answer"))?;
    let made = send(
        s.http_client()
            .post(format!("{url}/api/api-keys"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"name": KEY_NAME, "userId": user_id, "isActive": true})),
        "making an API key",
    )
    .await;
    end_session().await;
    let made = made?;
    let k = &made["apiKey"];
    Ok((
        str_of(k, "apiKey").ok_or_else(|| upstream("no API key in the answer"))?,
        str_of(k, "id").unwrap_or_default(),
    ))
}

/// `DELETE /v1/sources/audiobookshelf/server`: forgets the server (and deletes the key the node
/// made for itself).
pub(crate) async fn disconnect(State(s): State<Arc<NodeState>>) -> ApiResult<AbsView> {
    let current = s.audiobookshelf.read().expect("audiobookshelf").server.clone();
    if let Some(srv) = current {
        if let Some(id) = srv.key_id.as_deref().filter(|i| valid_id(i).is_ok()) {
            let _ = s
                .http_client()
                .delete(format!("{}/api/api-keys/{id}", base(&srv)))
                .bearer_auth(&srv.api_key)
                .send()
                .await;
        }
    }
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(SETTING)?;
    *s.audiobookshelf.write().expect("audiobookshelf") = Abs::default();
    Ok(Json(view(None)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Value {
        serde_json::json!({"id": "2fe18e1d-013a-432c-b12d-ea4739751c41", "mediaType": "book",
            "media": {"metadata": {"title": "A Short Book", "authorName": "Test Author", "publishedYear": "1999"},
                "coverPath": "/metadata/items/x/cover.jpg", "duration": 12.068572,
                "audioFiles": [{"index": 1, "ino": "84", "codec": "mp3", "bitRate": 64000}, {"index": 2, "ino": "85", "codec": "mp3"}],
                "chapters": [{"id": 0, "start": 0, "end": 6.03, "title": "Chapter 1"}],
                "tracks": [{"index": 1, "startOffset": 0, "duration": 6.034286}, {"index": 2, "startOffset": 6.034286, "duration": 6.034286}]}})
    }

    #[test]
    fn books_map_to_parts_and_positions() {
        let it = item();
        let h = hit(&it).unwrap();
        assert_eq!(
            (h.id.as_str(), h.kind.as_str(), h.year),
            ("book:2fe18e1d-013a-432c-b12d-ea4739751c41", "album", Some(1999))
        );
        assert_eq!(h.duration_ms, Some(12_069));
        assert_eq!(
            h.image.as_deref(),
            Some("/v1/sources/audiobookshelf/cover/2fe18e1d-013a-432c-b12d-ea4739751c41")
        );
        let b = book_of(&it);
        assert_eq!(b.parts.len(), 2);
        assert_eq!((b.parts[1].ino.as_str(), b.parts[1].start_ms), ("85", 6_034));
        assert_eq!(part_title(&it, &b.parts[0], 1), "Chapter 1");
        assert_eq!(part_title(&it, &b.parts[1], 2), "Part 2");
        assert_eq!(format_of(&it, "84"), "MP3 64 kbps (Audiobookshelf)");
        assert_eq!(locate(&b, 0), (0, 0));
        assert_eq!(locate(&b, 8_000), (1, 1_966));
        assert_eq!(book_position(&b, "85", 1_000), Some(7_034));
        assert!(hit(
            &serde_json::json!({"id": "p", "mediaType": "podcast", "media": {"metadata": {"title": "x"}}})
        )
        .is_none());
        assert_eq!(split_track("abc-1/84"), Some(("abc-1", "84")));
        assert!(split_track("../x/84").is_none());
        let srv = Server {
            api_key: "S3CRET".into(),
            ..Default::default()
        };
        assert!(!format!("{srv:?}").contains("S3CRET"));
    }
}
