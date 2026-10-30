//! Bandcamp, unofficial mode (STEWARD-DUAL-MODE.md item 5; owner: "mainly for downloading music
//! the user bought"). Off by default, behind a warning.
//!
//! - **Without an account:** search (Bandcamp's public autocomplete), album and track pages read
//!   from the page's own `data-tralbum`, and the streams Bandcamp's player plays (`mp3-128`; full
//!   tracks where the artist allows streaming, else none). Stream URLs carry a short-lived token, so
//!   the node resolves them when a track starts.
//! - **With the owner's sign-in** (the value of their `identity` cookie, pasted once, never shown
//!   again): their collection (purchases), and **downloads** of what they bought, in the format they
//!   pick (FLAC first), unpacked into `<data_dir>/bandcamp/<artist> - <title>/`, which is a library
//!   folder: bought music lands in the library like any file.
//!
//! Nothing is decrypted or circumvented: purchases download through the same links Bandcamp's own
//! download page offers the buyer. A page or API that no longer looks as expected switches the mode
//! off, with Try again.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub const SOURCE_ID: &str = "bandcamp";
pub const TRACK_URI_PREFIX: &str = "bandcamp:track:";
pub const STATE_KEY: &str = "bandcamp.unofficial";
/// The owner's `identity` cookie (a secret in backups).
pub const COOKIE_KEY: &str = "bandcamp.identity";
const SITE: &str = "https://bandcamp.com";
pub const WARNING: &str = "This reads Bandcamp's own web pages and private API, which Bandcamp has not approved for other apps. It can stop working at any time, and Bandcamp could act on the account whose sign-in you enter. Buying stays on bandcamp.com.";
/// Download formats, best first, as Bandcamp names them.
pub const FORMATS: &[&str] = &[
    "flac",
    "alac",
    "aiff-lossless",
    "wav",
    "mp3-320",
    "mp3-v0",
    "aac-hi",
    "vorbis",
];

// ---- where Bandcamp is (tests stand it in) -----------------------------------------------------

fn base_override() -> &'static std::sync::RwLock<Option<String>> {
    static B: std::sync::OnceLock<std::sync::RwLock<Option<String>>> = std::sync::OnceLock::new();
    B.get_or_init(|| std::sync::RwLock::new(None))
}

/// Tests: bandcamp.com stood in; band pages are then `<base>/_band/<band>/<kind>/<slug>`.
pub fn set_site_base(url: &str) {
    *base_override().write().expect("bandcamp base") = Some(url.trim_end_matches('/').to_string());
}

fn site() -> String {
    base_override()
        .read()
        .expect("bandcamp base")
        .clone()
        .unwrap_or_else(|| SITE.to_string())
}

/// `https://<band>.bandcamp.com/<kind>/<slug>`, the page to read.
fn page_url(band: &str, kind: &str, slug: &str) -> String {
    match base_override().read().expect("bandcamp base").as_deref() {
        Some(b) => format!("{b}/_band/{band}/{kind}/{slug}"),
        None => format!("https://{band}.bandcamp.com/{kind}/{slug}"),
    }
}

/// The public link (always the real one, for "open" and for bookmarks).
fn public_url(band: &str, kind: &str, slug: &str) -> String {
    format!("https://{band}.bandcamp.com/{kind}/{slug}")
}

fn now() -> i64 {
    pm_library::db::now_unix()
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("Bandcamp: {e}"))
}

fn word(s: &str) -> bool {
    !s.is_empty() && s.len() <= 120 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `(band, album|track, slug)` from a `*.bandcamp.com` link.
pub fn parse_link(url: &str) -> Option<(String, String, String)> {
    let rest = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let mut parts = rest.split(['/', '?', '#']);
    let host = parts.next()?;
    let band = host.strip_suffix(".bandcamp.com")?;
    let kind = parts.next()?;
    let slug = parts.next()?;
    (word(band) && (kind == "album" || kind == "track") && word(slug))
        .then(|| (band.to_string(), kind.to_string(), slug.to_string()))
}

/// A track's URI: `bandcamp:track:<band>.<a|t>.<slug>.<track number>`.
pub fn track_uri(band: &str, kind: &str, slug: &str, n: u64) -> String {
    let k = if kind == "album" { "a" } else { "t" };
    format!("{TRACK_URI_PREFIX}{band}.{k}.{slug}.{n}")
}

/// `(band, kind, slug, n)` from the part after `bandcamp:track:`.
fn parse_track_key(key: &str) -> Option<(String, String, String, u64)> {
    let mut p = key.split('.');
    let (band, k, slug, n) = (p.next()?, p.next()?, p.next()?, p.next()?);
    if p.next().is_some() || !word(band) || !word(slug) {
        return None;
    }
    let kind = match k {
        "a" => "album",
        "t" => "track",
        _ => return None,
    };
    Some((band.into(), kind.into(), slug.into(), n.parse().ok()?))
}

// ---- state -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Fan {
    pub fan_id: u64,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeState {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub health: Option<Health>,
    /// Who the sign-in belongs to, from `collection_summary`.
    #[serde(default)]
    pub fan: Option<Fan>,
    /// The download format to ask for first (one of [`FORMATS`]).
    #[serde(default)]
    pub format: String,
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

fn cookie_of(lib: &pm_library::Library) -> Option<String> {
    lib.get_setting(COOKIE_KEY)
        .ok()
        .flatten()
        .filter(|c| !c.is_empty())
}

pub fn on(s: &NodeState) -> bool {
    load(&s.library.lock().expect("library poisoned")).enabled
}

fn ready(s: &NodeState) -> Result<(), ApiError> {
    if on(s) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Bandcamp is off: switch it on under Sources › Bandcamp",
        ))
    }
}

fn signed_in(s: &NodeState) -> Result<(String, Fan), ApiError> {
    ready(s)?;
    let lib = s.library.lock().expect("library poisoned");
    let cookie = cookie_of(&lib).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Enter your Bandcamp sign-in first (Sources › Bandcamp)",
        )
    })?;
    let fan = load(&lib).fan.ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Enter your Bandcamp sign-in again (Sources › Bandcamp)",
        )
    })?;
    Ok((cookie, fan))
}

fn mark_down(library: &std::sync::Mutex<pm_library::Library>, why: &str) {
    tracing::warn!("Bandcamp (unofficial) switched off: {why}");
    if let Ok(lib) = library.lock() {
        let mut st = load(&lib);
        st.enabled = false;
        st.health = Some(Health {
            ok: false,
            detail: why.to_string(),
            at: now(),
        });
        save(&lib, &st);
    }
    crate::search::forget_answers();
}

fn mark_ok(library: &std::sync::Mutex<pm_library::Library>, what: &str) {
    if let Ok(lib) = library.lock() {
        let mut st = load(&lib);
        st.health = Some(Health {
            ok: true,
            detail: what.into(),
            at: now(),
        });
        save(&lib, &st);
    }
}

// ---- pages -------------------------------------------------------------------------------------

fn unescape(t: &str) -> String {
    t.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The JSON in an HTML attribute (`data-tralbum`, `data-blob`): `None` when the page has none.
pub fn attr_json(html: &str, attr: &str) -> Option<Value> {
    let start = html.find(&format!("{attr}=\""))? + attr.len() + 2;
    let end = start + html[start..].find('"')?;
    serde_json::from_str(&unescape(&html[start..end])).ok()
}

/// Whether a failure is the mode's (switch it off) or only this item's.
#[derive(Debug)]
pub enum Failure {
    Mode(String),
    Item(String),
}

fn judge(status: u16, what: &str) -> Result<(), Failure> {
    match status {
        200..=299 => Ok(()),
        404 | 410 => Err(Failure::Item(format!("Bandcamp has no {what}"))),
        429 => Err(Failure::Item(
            "Bandcamp is rate-limiting this node; try again later".into(),
        )),
        403 => Err(Failure::Mode(format!(
            "Bandcamp refused this node ({status}) for {what}"
        ))),
        s if s >= 500 => Err(Failure::Item(format!("Bandcamp answered {s} for {what}"))),
        s => Err(Failure::Mode(format!("Bandcamp answered {s} for {what}"))),
    }
}

/// An album or track page's `data-tralbum`.
async fn tralbum(s: &NodeState, band: &str, kind: &str, slug: &str) -> Result<Value, Failure> {
    let resp = s
        .http_client()
        .get(page_url(band, kind, slug))
        .send()
        .await
        .map_err(|e| Failure::Item(format!("Bandcamp: {}", e.without_url())))?;
    judge(resp.status().as_u16(), &format!("{kind} {band}/{slug}"))?;
    let html = resp
        .text()
        .await
        .map_err(|e| Failure::Item(format!("Bandcamp: {e}")))?;
    attr_json(&html, "data-tralbum")
        .ok_or_else(|| Failure::Mode("Bandcamp's pages changed: no track data on an album page".into()))
}

fn tralbum_blocking(band: &str, kind: &str, slug: &str) -> Result<Value, Failure> {
    let resp = crate::sources::http_client()
        .get(page_url(band, kind, slug))
        .send()
        .map_err(|e| Failure::Item(format!("Bandcamp: {}", e.without_url())))?;
    judge(resp.status().as_u16(), &format!("{kind} {band}/{slug}"))?;
    let html = resp.text().map_err(|e| Failure::Item(format!("Bandcamp: {e}")))?;
    attr_json(&html, "data-tralbum")
        .ok_or_else(|| Failure::Mode("Bandcamp's pages changed: no track data on an album page".into()))
}

fn year_of(d: &str) -> Option<i32> {
    d.split([' ', '-', 'T'])
        .find(|p| p.len() == 4 && p.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|p| p.parse().ok())
}

fn art(id: Option<u64>) -> Option<String> {
    id.filter(|a| *a > 0)
        .map(|a| format!("https://f4.bcbits.com/img/a{a}_10.jpg"))
}

/// The stream of a track in a tralbum, `https:` added where Bandcamp leaves it out.
fn stream_of(t: &Value) -> Option<String> {
    let u = t["file"]["mp3-128"].as_str()?;
    if let Some(r) = u.strip_prefix("//") {
        Some(format!("https://{r}"))
    } else {
        (u.starts_with("https://") || u.starts_with("http://")).then(|| u.to_string())
    }
}

/// A page's tralbum as an item: its tracks that Bandcamp streams.
pub fn item_from(band: &str, kind: &str, slug: &str, d: &Value) -> SourceItem {
    let cur = &d["current"];
    let artist = d["artist"].as_str().map(str::to_string);
    let tracks = d["trackinfo"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, t)| {
            stream_of(t)?;
            let n = t["track_num"].as_u64().unwrap_or(i as u64 + 1);
            Some(SourceTrack {
                title: t["title"].as_str()?.to_string(),
                track_no: Some(n as u32),
                duration_ms: t["duration"].as_f64().map(|x| (x * 1000.0) as u64),
                format: "MP3 128 kbit/s (Bandcamp stream)".into(),
                url: track_uri(band, kind, slug, n),
                artist: t["artist"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .or_else(|| artist.clone()),
            })
        })
        .collect();
    SourceItem {
        source: SOURCE_ID.into(),
        id: format!("{kind}:{band}.{slug}"),
        title: cur["title"].as_str().unwrap_or(slug).to_string(),
        creator: artist,
        year: cur["release_date"]
            .as_str()
            .or(d["album_release_date"].as_str())
            .and_then(year_of),
        description: cur["about"]
            .as_str()
            .filter(|a| !a.trim().is_empty())
            .map(str::to_string),
        licence: None,
        image: art(d["art_id"].as_u64()),
        url: public_url(band, kind, slug),
        tracks,
        library_album_id: None,
    }
}

fn hit_from(r: &Value) -> Option<SourceHit> {
    let kind = match r["type"].as_str()? {
        "a" => "album",
        "t" => "track",
        _ => return None,
    };
    let link = r["item_url_path"].as_str()?;
    let (band, k, slug) = parse_link(link)?;
    if k != kind {
        return None;
    }
    Some(SourceHit {
        id: format!("{kind}:{band}.{slug}"),
        title: r["name"].as_str()?.to_string(),
        creator: r["band_name"].as_str().map(str::to_string),
        year: None,
        downloads: None,
        image: r["img"].as_str().map(|u| u.replace("_3.jpg", "_10.jpg")),
        kind: if kind == "album" {
            "item".into()
        } else {
            "track".into()
        },
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: public_url(&band, kind, &slug),
    })
}

pub struct Bandcamp;

#[async_trait::async_trait]
impl StreamingService for Bandcamp {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play", "library", "downloads"]
    }

    fn stage(&self) -> &'static str {
        "unofficial"
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let lib = s.library.lock().expect("library poisoned");
        let st = load(&lib);
        let status = if let Some(h) = st.health.as_ref().filter(|h| !h.ok && !st.enabled) {
            format!("error: {}", h.detail)
        } else if !st.enabled {
            "off: switch Bandcamp (unofficial) on under Sources › Bandcamp".to_string()
        } else if cookie_of(&lib).is_some() && st.fan.is_some() {
            "signed in".to_string()
        } else {
            "ready".to_string()
        };
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Bandcamp".into(),
            description: "Unofficial: search and Bandcamp's own streams; with your sign-in, your purchases downloaded into the library".into(),
            status,
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        ready(s)?;
        if let Some((band, kind, slug)) = parse_link(term) {
            return Ok(vec![SourceHit {
                id: format!("{kind}:{band}.{slug}"),
                title: slug.replace('-', " "),
                creator: Some(band.clone()),
                year: None,
                downloads: None,
                image: None,
                kind: if kind == "album" {
                    "item".into()
                } else {
                    "track".into()
                },
                duration_ms: None,
                library_artist_id: None,
                isrc: None,
                upc: None,
                library_track_id: None,
                library_album_id: None,
                uri: None,
                url: public_url(&band, &kind, &slug),
            }]);
        }
        let resp = s
            .http_client()
            .post(format!("{}/api/bcsearch_public_api/1/autocomplete_elastic", site()))
            .json(&serde_json::json!({"search_text": term, "search_filter": "", "full_page": false, "fan_id": null}))
            .send()
            .await
            .map_err(|e| upstream(e.without_url()))?;
        if let Err(f) = judge(resp.status().as_u16(), "search") {
            return Err(match f {
                Failure::Mode(m) => {
                    mark_down(&s.library, &m);
                    upstream(m)
                }
                Failure::Item(m) => upstream(m),
            });
        }
        let v: Value = resp.json().await.map_err(upstream)?;
        let Some(results) = v["auto"]["results"].as_array() else {
            let m = "Bandcamp's search changed: no results list";
            mark_down(&s.library, m);
            return Err(upstream(m));
        };
        Ok(results
            .iter()
            .filter_map(hit_from)
            .take(limit.clamp(1, 50) as usize)
            .collect())
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        ready(s)?;
        let (kind, rest) = id
            .split_once(':')
            .ok_or_else(|| ApiError::bad_request("bad Bandcamp id"))?;
        let (band, slug) = rest
            .split_once('.')
            .ok_or_else(|| ApiError::bad_request("bad Bandcamp id"))?;
        if !(kind == "album" || kind == "track") || !word(band) || !word(slug) {
            return Err(ApiError::bad_request("bad Bandcamp id"));
        }
        match tralbum(s, band, kind, slug).await {
            Ok(d) => Ok(item_from(band, kind, slug, &d)),
            Err(Failure::Mode(m)) => {
                mark_down(&s.library, &m);
                Err(upstream(m))
            }
            Err(Failure::Item(m)) => Err(ApiError::not_found(m)),
        }
    }
}

// ---- streams -----------------------------------------------------------------------------------

/// The track's current stream URL (Bandcamp's tokens expire, so read it when it starts).
fn pick_track(d: &Value, n: u64) -> Result<String, Failure> {
    let list = d["trackinfo"].as_array().cloned().unwrap_or_default();
    let t = list
        .iter()
        .enumerate()
        .find(|(i, t)| t["track_num"].as_u64().unwrap_or(*i as u64 + 1) == n)
        .map(|(_, t)| t)
        .or_else(|| (list.len() == 1).then(|| &list[0]))
        .ok_or_else(|| Failure::Item(format!("Bandcamp has no track {n} there")))?;
    stream_of(t).ok_or_else(|| Failure::Item("the artist doesn't let Bandcamp stream this track".into()))
}

fn record(library: &std::sync::Mutex<pm_library::Library>, r: &Result<String, Failure>) {
    match r {
        Ok(_) => mark_ok(library, "tracks are playing"),
        Err(Failure::Mode(m)) => mark_down(library, m),
        Err(Failure::Item(_)) => {}
    }
}

/// A `bandcamp:track:…` for a zone.
pub(crate) fn open_track_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    key: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let uri = format!("{TRACK_URI_PREFIX}{key}");
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: uri.clone(),
        reason,
    };
    let (band, kind, slug, n) = parse_track_key(key).ok_or_else(|| fail("bad Bandcamp track".into()))?;
    if !library.lock().is_ok_and(|l| load(&l).enabled) {
        return Err(fail("Bandcamp is off (Sources › Bandcamp)".into()));
    }
    let r = tralbum_blocking(&band, &kind, &slug).and_then(|d| pick_track(&d, n));
    record(library, &r);
    let url = r.map_err(|e| match e {
        Failure::Mode(m) | Failure::Item(m) => fail(m),
    })?;
    crate::sources::open_http_stream(&url).map_err(|_| fail("the Bandcamp stream did not open".into()))
}

/// `GET /v1/sources/bandcamp/stream/{key}`: the track for browsers, relayed (fresh token each time).
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    Path(key): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    ready(&s)?;
    let (band, kind, slug, n) =
        parse_track_key(&key).ok_or_else(|| ApiError::bad_request("bad Bandcamp track"))?;
    let r = match tralbum(&s, &band, &kind, &slug).await {
        Ok(d) => pick_track(&d, n),
        Err(e) => Err(e),
    };
    record(&s.library, &r);
    let url = r.map_err(|e| match e {
        Failure::Mode(m) => upstream(m),
        Failure::Item(m) => ApiError::not_found(m),
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
    let mut out = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "audio/mpeg")
        .header(axum::http::header::CACHE_CONTROL, "private, max-age=300");
    if let Some(n) = resp.content_length() {
        out = out.header(axum::http::header::CONTENT_LENGTH, n);
    }
    out.body(axum::body::Body::from_stream(resp.bytes_stream()))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ---- the owner's collection --------------------------------------------------------------------

fn with_cookie(r: reqwest::RequestBuilder, cookie: &str) -> reqwest::RequestBuilder {
    r.header(reqwest::header::COOKIE, format!("identity={cookie}"))
}

/// Who the sign-in belongs to.
async fn fan_of(s: &NodeState, cookie: &str) -> Result<Fan, ApiError> {
    let resp = with_cookie(
        s.http_client()
            .get(format!("{}/api/fan/2/collection_summary", site())),
        cookie,
    )
    .send()
    .await
    .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "Bandcamp didn't take the sign-in ({})",
            resp.status()
        )));
    }
    let v: Value = resp.json().await.map_err(upstream)?;
    let fan_id = v["fan_id"]
        .as_u64()
        .or(v["collection_summary"]["fan_id"].as_u64())
        .ok_or_else(|| {
            ApiError::bad_request("Bandcamp didn't recognise the sign-in: copy the identity cookie again")
        })?;
    Ok(Fan {
        fan_id,
        username: v["collection_summary"]["username"].as_str().map(str::to_string),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Purchase {
    /// `<sale_item_type><sale_item_id>`, Bandcamp's own key for the download.
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// `album` or `track`.
    pub kind: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The item's page here (`album:<band>.<slug>`), when its link is a bandcamp.com one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub downloadable: bool,
    /// Already downloaded into the library (its folder exists).
    pub downloaded: bool,
}

fn purchases_dir(s: &NodeState) -> PathBuf {
    s.data_dir().join("bandcamp")
}

fn safe_name(t: &str) -> String {
    let c: String = t
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let c = c.trim().trim_matches('.').trim();
    if c.is_empty() {
        "Untitled".into()
    } else {
        c.chars().take(120).collect()
    }
}

fn folder_for(s: &NodeState, artist: Option<&str>, title: &str) -> PathBuf {
    purchases_dir(s).join(safe_name(&format!(
        "{} - {title}",
        artist.unwrap_or("Unknown artist")
    )))
}

/// The whole collection: items and their download pages.
async fn collection_of(
    s: &NodeState,
    cookie: &str,
    fan: &Fan,
) -> Result<(Vec<Purchase>, HashMap<String, String>), ApiError> {
    let mut out = Vec::new();
    let mut urls: HashMap<String, String> = HashMap::new();
    let mut token = format!("{}::a::", now() + 86_400);
    for _ in 0..50 {
        let resp = with_cookie(
            s.http_client()
                .post(format!("{}/api/fancollection/1/collection_items", site())),
            cookie,
        )
        .json(&serde_json::json!({"fan_id": fan.fan_id, "older_than_token": token, "count": 100}))
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
        if let Err(f) = judge(resp.status().as_u16(), "your collection") {
            return Err(match f {
                Failure::Mode(m) => {
                    mark_down(&s.library, &m);
                    upstream(m)
                }
                Failure::Item(m) => upstream(m),
            });
        }
        let v: Value = resp.json().await.map_err(upstream)?;
        if let Some(m) = v["redownload_urls"].as_object() {
            for (k, u) in m {
                if let Some(u) = u.as_str() {
                    urls.insert(k.clone(), u.to_string());
                }
            }
        }
        for it in v["items"].as_array().cloned().unwrap_or_default() {
            let key = format!(
                "{}{}",
                it["sale_item_type"].as_str().unwrap_or(""),
                it["sale_item_id"].as_u64().unwrap_or(0)
            );
            let title = it["item_title"].as_str().unwrap_or("").to_string();
            let artist = it["band_name"].as_str().map(str::to_string);
            let url = it["item_url"].as_str().unwrap_or("").to_string();
            let item = parse_link(&url).map(|(b, k, sl)| format!("{k}:{b}.{sl}"));
            out.push(Purchase {
                downloaded: folder_for(s, artist.as_deref(), &title).is_dir(),
                downloadable: false,
                kind: it["item_type"].as_str().unwrap_or("album").to_string(),
                image: it["item_art_url"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| art(it["item_art_id"].as_u64())),
                id: key,
                title,
                artist,
                url,
                item,
            });
        }
        let more = v["more_available"].as_bool().unwrap_or(false);
        match v["last_token"].as_str() {
            Some(t) if more && t != token => token = t.to_string(),
            _ => break,
        }
    }
    for p in &mut out {
        p.downloadable = urls.contains_key(&p.id);
    }
    Ok((out, urls))
}

/// `GET /v1/sources/bandcamp/collection`: the owner's purchases.
pub(crate) async fn collection(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<Purchase>> {
    let (cookie, fan) = signed_in(&s)?;
    Ok(Json(collection_of(&s, &cookie, &fan).await?.0))
}

// ---- downloads ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct DownloadJob {
    pub id: String,
    pub title: String,
    /// `queued`, `downloading`, `unpacking`, `done` or `failed`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub at: i64,
}

fn jobs() -> &'static std::sync::Mutex<HashMap<String, DownloadJob>> {
    static J: std::sync::OnceLock<std::sync::Mutex<HashMap<String, DownloadJob>>> =
        std::sync::OnceLock::new();
    J.get_or_init(Default::default)
}

fn set_job(id: &str, f: impl FnOnce(&mut DownloadJob)) {
    if let Some(j) = jobs().lock().expect("bandcamp jobs").get_mut(id) {
        f(j);
        j.at = now();
    }
}

/// `GET /v1/sources/bandcamp/downloads`: this run's downloads, newest first.
pub(crate) async fn downloads(State(_s): State<Arc<NodeState>>) -> ApiResult<Vec<DownloadJob>> {
    let mut v: Vec<DownloadJob> = jobs().lock().expect("bandcamp jobs").values().cloned().collect();
    v.sort_by_key(|j| std::cmp::Reverse(j.at));
    Ok(Json(v))
}

/// The format to fetch: the owner's choice when offered, else the best one offered.
pub fn choose_format(offered: &serde_json::Map<String, Value>, want: &str) -> Option<String> {
    let has = |f: &str| offered.get(f).and_then(|d| d["url"].as_str()).is_some();
    if !want.is_empty() && has(want) {
        return Some(want.to_string());
    }
    FORMATS.iter().find(|f| has(f)).map(|f| f.to_string())
}

/// Bandcamp's download link → the file's URL (the `statdownload` step its own page takes).
fn stat_url(download: &str) -> String {
    let mut u = download.replacen("/download/", "/statdownload/", 1);
    // Bandcamp's own hosts always answer over TLS
    let host = u.split('/').nth(2).unwrap_or("");
    if u.starts_with("http://") && (host.ends_with(".bcbits.com") || host.ends_with("bandcamp.com")) {
        u = u.replacen("http://", "https://", 1);
    }
    let sep = if u.contains('?') { '&' } else { '?' };
    format!("{u}{sep}.vrs=1&.rand={}", rand::random::<u32>())
}

/// `{"download_url": …}` inside `Downloads.statResult( … )`, or plain JSON.
pub fn stat_result(body: &str) -> Option<String> {
    let start = body.find("{\"")?;
    let v: Value = body
        .match_indices('}')
        .rev()
        .filter(|(i, _)| *i > start)
        .find_map(|(i, _)| serde_json::from_str(&body[start..=i]).ok())?;
    v["download_url"]
        .as_str()
        .or(v["retry_url"].as_str())
        .or(v["url"].as_str())
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(str::to_string)
}

/// Unpack a downloaded zip into `dest`, entries kept inside it.
fn unzip(file: &std::path::Path, dest: &std::path::Path) -> Result<usize, String> {
    let f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("not a zip Bandcamp sends: {e}"))?;
    let mut n = 0;
    for i in 0..z.len() {
        let mut e = z.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = e.enclosed_name() else { continue };
        let out = dest.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let mut w = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut e, &mut w).map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(n)
}

#[derive(Debug, Deserialize, Default)]
pub struct DownloadRequest {
    #[serde(default)]
    pub format: Option<String>,
}

/// `POST /v1/sources/bandcamp/collection/{id}/download {format?}`: download one purchase into the
/// library (a job; `GET …/downloads` shows it).
pub(crate) async fn download(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    raw: axum::body::Bytes,
) -> ApiResult<DownloadJob> {
    let body: Option<DownloadRequest> = if raw.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        Some(
            serde_json::from_slice(&raw)
                .map_err(|e| ApiError::bad_request(format!("bad request body: {e}")))?,
        )
    };
    let (cookie, fan) = signed_in(&s)?;
    if id.len() > 40 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ApiError::bad_request("bad Bandcamp purchase"));
    }
    if let Some(f) = body.as_ref().and_then(|b| b.format.as_deref()) {
        if !FORMATS.contains(&f) {
            return Err(ApiError::bad_request("unknown download format"));
        }
    }
    if jobs()
        .lock()
        .expect("bandcamp jobs")
        .get(&id)
        .is_some_and(|j| !matches!(j.state.as_str(), "done" | "failed"))
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "That download is already running",
        ));
    }
    let (items, urls) = collection_of(&s, &cookie, &fan).await?;
    let p = items
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("That isn't in your Bandcamp collection"))?;
    let page = urls.get(&id).cloned().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "Bandcamp offers no download for this purchase",
        )
    })?;
    let want = body
        .and_then(|b| b.format)
        .unwrap_or_else(|| load(&s.library.lock().expect("library poisoned")).format);
    let job = DownloadJob {
        id: id.clone(),
        title: format!("{} - {}", p.artist.clone().unwrap_or_default(), p.title),
        state: "queued".into(),
        at: now(),
        ..Default::default()
    };
    jobs()
        .lock()
        .expect("bandcamp jobs")
        .insert(id.clone(), job.clone());
    let s2 = s.clone();
    tokio::spawn(async move {
        let r = run_download(&s2, &id, &cookie, &page, &want, &p).await;
        match r {
            Ok(folder) => {
                set_job(&id, |j| {
                    j.state = "done".into();
                    j.folder = Some(folder.display().to_string());
                });
                mark_ok(&s2.library, "purchases download");
                add_to_library(&s2).await;
            }
            Err(e) => {
                tracing::warn!(purchase = %id, "Bandcamp download failed: {e}");
                set_job(&id, |j| {
                    j.state = "failed".into();
                    j.error = Some(e);
                });
            }
        }
    });
    Ok(Json(job))
}

async fn run_download(
    s: &Arc<NodeState>,
    id: &str,
    cookie: &str,
    page: &str,
    want: &str,
    p: &Purchase,
) -> Result<PathBuf, String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    set_job(id, |j| j.state = "downloading".into());
    let html = with_cookie(s.http_client().get(page), cookie)
        .send()
        .await
        .map_err(|e| format!("Bandcamp: {}", e.without_url()))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let blob = attr_json(&html, "data-blob").ok_or("Bandcamp's download page changed: no download data")?;
    let digital = &blob["digital_items"][0];
    let offered = digital["downloads"]
        .as_object()
        .ok_or("Bandcamp offers no files for this purchase yet")?;
    let format = choose_format(offered, want).ok_or("Bandcamp offers no format the node knows")?;
    let link = offered[&format]["url"].as_str().unwrap_or_default().to_string();
    set_job(id, |j| j.format = Some(format.clone()));
    let stat = s
        .http_client()
        .get(stat_url(&link))
        .send()
        .await
        .map_err(|e| format!("Bandcamp: {}", e.without_url()))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let file_url =
        stat_result(&stat).ok_or("Bandcamp didn't hand out the file (its download step changed)")?;
    let resp = s
        .http_client()
        .get(&file_url)
        .send()
        .await
        .map_err(|e| format!("Bandcamp: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("the file answered {}", resp.status()));
    }
    let total = resp.content_length();
    set_job(id, |j| j.total = total);
    let dir = purchases_dir(s);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let part = dir.join(format!(".{id}.part"));
    let mut f = tokio::fs::File::create(&part).await.map_err(|e| e.to_string())?;
    let mut got = 0u64;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("Bandcamp: {}", e.without_url()))?;
        f.write_all(&chunk).await.map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        set_job(id, |j| j.bytes = got);
    }
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    let artist = digital["artist"]
        .as_str()
        .map(str::to_string)
        .or(p.artist.clone());
    let title = digital["title"]
        .as_str()
        .map(str::to_string)
        .unwrap_or(p.title.clone());
    let folder = folder_for(s, artist.as_deref(), &title);
    set_job(id, |j| j.state = "unpacking".into());
    let (part2, folder2) = (part.clone(), folder.clone());
    let ext = format_ext(&format).to_string();
    let title2 = title.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        std::fs::create_dir_all(&folder2).map_err(|e| e.to_string())?;
        let mut head = [0u8; 4];
        let is_zip = std::fs::File::open(&part2)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
            .is_ok()
            && head == *b"PK\x03\x04";
        let r = if is_zip {
            unzip(&part2, &folder2).map(|_| ())
        } else {
            std::fs::rename(&part2, folder2.join(format!("{}.{ext}", safe_name(&title2))))
                .map_err(|e| e.to_string())
        };
        let _ = std::fs::remove_file(&part2);
        r
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(folder)
}

fn format_ext(f: &str) -> &'static str {
    match f {
        "flac" => "flac",
        "alac" | "aac-hi" => "m4a",
        "aiff-lossless" => "aiff",
        "wav" => "wav",
        "vorbis" => "ogg",
        _ => "mp3",
    }
}

/// `<data_dir>/bandcamp` becomes a library folder (once), and is scanned.
async fn add_to_library(s: &Arc<NodeState>) {
    let Ok(dir) = tokio::fs::canonicalize(purchases_dir(s)).await else {
        return;
    };
    let known = s
        .library
        .lock()
        .expect("library poisoned")
        .list_folders()
        .map(|f| f.into_iter().any(|(p, _)| p == dir))
        .unwrap_or(false);
    if !known {
        if s.library
            .lock()
            .expect("library poisoned")
            .add_folder(&dir)
            .is_err()
        {
            return;
        }
        s.watch_more(&dir);
        tracing::info!(dir = %dir.display(), "Bandcamp purchases are a library folder");
    }
    let _ = s.start_scan(vec![dir]);
}

// ---- the tab -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct BandcampStatus {
    pub enabled: bool,
    pub signed_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub format: String,
    pub formats: Vec<String>,
    /// Where purchases go (a library folder once the first one lands).
    pub folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    pub warning: String,
}

fn view(s: &NodeState) -> BandcampStatus {
    let lib = s.library.lock().expect("library poisoned");
    let st = load(&lib);
    BandcampStatus {
        enabled: st.enabled,
        signed_in: cookie_of(&lib).is_some() && st.fan.is_some(),
        user: st.fan.as_ref().and_then(|f| f.username.clone()),
        format: if st.format.is_empty() {
            "flac".into()
        } else {
            st.format.clone()
        },
        formats: FORMATS.iter().map(|f| f.to_string()).collect(),
        folder: purchases_dir(s).display().to_string(),
        health: st.health,
        warning: WARNING.into(),
    }
}

/// `GET /v1/sources/bandcamp/unofficial`.
pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<BandcampStatus> {
    Ok(Json(view(&s)))
}

#[derive(Debug, Deserialize)]
pub struct BandcampUpdate {
    pub enabled: Option<bool>,
    /// The owner's `identity` cookie; empty signs out. Checked with Bandcamp before it is kept.
    pub identity: Option<String>,
    pub format: Option<String>,
}

/// `PUT /v1/sources/bandcamp/unofficial {enabled?, identity?, format?}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<BandcampUpdate>,
) -> ApiResult<BandcampStatus> {
    if let Some(f) = u.format.as_deref() {
        if !FORMATS.contains(&f) {
            return Err(ApiError::bad_request("unknown download format"));
        }
    }
    if let Some(c) = u.identity.as_deref() {
        let c = c.trim().trim_start_matches("identity=").trim();
        if c.is_empty() {
            s.library
                .lock()
                .expect("library poisoned")
                .delete_setting(COOKIE_KEY)?;
            update(&s, |st| st.fan = None);
        } else {
            if c.len() > 1024 || !c.bytes().all(|b| b.is_ascii_graphic() && b != b';') {
                return Err(ApiError::bad_request(
                    "Paste only the value of Bandcamp's identity cookie",
                ));
            }
            let fan = fan_of(&s, c).await?;
            {
                // The cookie, who it is and the "new sign-in" mark under one guard: no vault
                // pass sees one without the others
                let lib = s.library.lock().expect("library poisoned");
                lib.set_setting(COOKIE_KEY, c)?;
                let mut st = load(&lib);
                st.fan = Some(fan);
                save(&lib, &st);
                crate::vault_cache::new_sign_in(&lib, "bandcamp");
            }
        }
    }
    update(&s, |st| {
        if let Some(e) = u.enabled {
            st.enabled = e;
            if e {
                st.health = None;
            }
        }
        if let Some(f) = u.format {
            st.format = f;
        }
    });
    crate::search::forget_answers();
    Ok(Json(view(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_tracks_and_pages_parse() {
        assert_eq!(
            parse_link("https://daftpunk.bandcamp.com/album/ram?from=search"),
            Some(("daftpunk".into(), "album".into(), "ram".into()))
        );
        assert!(parse_link("https://example.com/album/ram").is_none());
        assert!(parse_link("https://daftpunk.bandcamp.com/merch/shirt").is_none());
        let u = track_uri("daftpunk", "album", "ram", 3);
        assert_eq!(u, "bandcamp:track:daftpunk.a.ram.3");
        assert_eq!(
            parse_track_key(u.strip_prefix(TRACK_URI_PREFIX).unwrap()),
            Some(("daftpunk".into(), "album".into(), "ram".into(), 3))
        );
        assert!(parse_track_key("a.b.c").is_none() && parse_track_key("x.q.y.1").is_none());
        let html = r#"<div id="x" data-tralbum="{&quot;artist&quot;:&quot;Daft Punk&quot;,&quot;art_id&quot;:12,&quot;album_release_date&quot;:&quot;16 Jan 2016 00:00:00 GMT&quot;,&quot;current&quot;:{&quot;title&quot;:&quot;R &amp; M&quot;},&quot;trackinfo&quot;:[{&quot;title&quot;:&quot;One&quot;,&quot;track_num&quot;:1,&quot;duration&quot;:60.5,&quot;file&quot;:{&quot;mp3-128&quot;:&quot;//t4.bcbits.com/stream/1?token=x&quot;}},{&quot;title&quot;:&quot;Two&quot;,&quot;track_num&quot;:2,&quot;file&quot;:null}]}"></div>"#;
        let d = attr_json(html, "data-tralbum").unwrap();
        let it = item_from("daftpunk", "album", "ram", &d);
        assert_eq!(
            (it.title.as_str(), it.year, it.tracks.len()),
            ("R & M", Some(2016), 1)
        );
        assert_eq!(it.tracks[0].url, "bandcamp:track:daftpunk.a.ram.1");
        assert_eq!(it.tracks[0].duration_ms, Some(60_500));
        assert_eq!(it.image.as_deref(), Some("https://f4.bcbits.com/img/a12_10.jpg"));
        assert_eq!(
            pick_track(&d, 1).unwrap(),
            "https://t4.bcbits.com/stream/1?token=x"
        );
        assert!(matches!(pick_track(&d, 2), Err(Failure::Item(_))));
    }

    #[test]
    fn downloads_pick_the_format_and_read_the_stat_result() {
        let offered = serde_json::json!({"mp3-320": {"url": "https://p4.bcbits.com/download/album/x"}, "flac": {"url": "https://p4.bcbits.com/download/album/y"}});
        let o = offered.as_object().unwrap();
        assert_eq!(choose_format(o, "").as_deref(), Some("flac"));
        assert_eq!(choose_format(o, "mp3-320").as_deref(), Some("mp3-320"));
        assert_eq!(choose_format(o, "wav").as_deref(), Some("flac"));
        let u = stat_url("http://p4.bcbits.com/download/album/y?id=1");
        assert!(
            u.starts_with("https://p4.bcbits.com/statdownload/album/y?id=1&.vrs=1&.rand="),
            "{u}"
        );
        assert_eq!(
            stat_result(r#"if ( window.Downloads ) { Downloads.statResult ( {"result":"ok","download_url":"https://p4.bcbits.com/a.zip"} ) };"#).as_deref(),
            Some("https://p4.bcbits.com/a.zip")
        );
        assert!(stat_result("nothing").is_none());
        assert_eq!(safe_name("AC/DC: Live?"), "AC_DC_ Live_");
    }
}
