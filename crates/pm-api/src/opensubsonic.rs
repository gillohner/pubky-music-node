//! Your own (Open)Subsonic server as a source (STEWARD-PLAN step 5.1): Navidrome, Airsonic, Gonic,
//! Ampache, LMS or another Pubky Music node. One server per node: its URL, a user and a password,
//! kept in the node's database and never returned by the API. Every request signs itself with a
//! fresh salt (`t` = md5(password + salt), the protocol's token auth), so no password travels.
//!
//! Songs queue as `subsonic:track:<id>`; the signed stream URL is made only when a song plays.
//! Covers and browser streams are relayed by the node. Songs carry their MusicBrainz id and ISRC
//! when the server knows them (OpenSubsonic), which is how playlists and favourites from the
//! server become canonical (ADR 0001).

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use md5::{Digest, Md5};
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub const SOURCE_ID: &str = "subsonic";
pub const TRACK_URI_PREFIX: &str = "subsonic:track:";
const SETTING: &str = "opensubsonic.server";
const CLIENT: &str = "pubky-music-node";
const API_VERSION: &str = "1.16.1";

/// The connected server. `Debug` never shows the password.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Server {
    pub url: String,
    pub user: String,
    pub password: String,
    /// What `ping` said: `navidrome`, `gonic`, … (OpenSubsonic `type`), and its version.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &"<set>")
            .field("kind", &self.kind)
            .finish()
    }
}

pub type Shared = Arc<RwLock<Option<Server>>>;

/// The server saved in the library, for the node's start.
pub fn load(lib: &pm_library::Library) -> Option<Server> {
    lib.get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

pub fn track_uri(id: &str) -> String {
    format!("{TRACK_URI_PREFIX}{id}")
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("your Subsonic server: {e}"))
}

/// Subsonic ids are opaque; anything URL-safe and short is accepted.
fn valid_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ApiError::bad_request("bad Subsonic id"));
    }
    Ok(id)
}

fn server(s: &NodeState) -> Result<Server, ApiError> {
    s.opensubsonic
        .read()
        .expect("opensubsonic")
        .clone()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_FAILED,
                "connect your Subsonic server first (Sources › Your server)",
            )
        })
}

pub(crate) fn connected(s: &NodeState) -> bool {
    s.opensubsonic.read().expect("opensubsonic").is_some()
}

/// The signed URL of a method (`stream`, `getCoverArt`, …) with its parameters.
fn signed_url(srv: &Server, method: &str, params: &[(&str, &str)]) -> Result<url::Url, String> {
    let salt = crate::tidal::random_urlsafe(12);
    let mut h = Md5::new();
    h.update(srv.password.as_bytes());
    h.update(salt.as_bytes());
    let token = hex::encode(h.finalize());
    let mut u = url::Url::parse(&format!("{}/rest/{method}", srv.url.trim_end_matches('/')))
        .map_err(|e| e.to_string())?;
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("u", &srv.user)
            .append_pair("t", &token)
            .append_pair("s", &salt)
            .append_pair("v", API_VERSION)
            .append_pair("c", CLIENT)
            .append_pair("f", "json");
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Ok(u)
}

/// One call; the server answers 200 with `subsonic-response.status` = `failed` on errors.
async fn call(s: &NodeState, srv: &Server, method: &str, params: &[(&str, &str)]) -> Result<Value, ApiError> {
    let url = signed_url(srv, method, params).map_err(upstream)?;
    let resp = s
        .http_client()
        .get(url)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream(format!("{method} answered {}", resp.status())));
    }
    let v: Value = resp.json().await.map_err(|e| upstream(e.without_url()))?;
    let r = v["subsonic-response"].clone();
    if r["status"].as_str() != Some("ok") {
        return Err(upstream(
            r["error"]["message"]
                .as_str()
                .unwrap_or("the server refused the request"),
        ));
    }
    Ok(r)
}

fn cover_path(id: &str) -> String {
    format!("/v1/sources/subsonic/cover/{id}")
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    match &v[k] {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The song's canonical identity: its MusicBrainz recording, else its first ISRC.
pub(crate) fn canonical_song(v: &Value) -> Option<pm_model::MusicUri> {
    if let Some(m) = v["musicBrainzId"]
        .as_str()
        .and_then(|m| pm_model::Mbid::parse(m).ok())
    {
        return Some(pm_model::MusicUri::recording(m));
    }
    let isrc = match &v["isrc"] {
        Value::Array(a) => a.iter().find_map(Value::as_str),
        Value::String(s) => Some(s.as_str()),
        _ => None,
    }?;
    pm_model::Isrc::parse(isrc)
        .ok()
        .map(|i| pm_model::MusicUri::Isrc { isrc: i })
}

fn first_isrc(v: &Value) -> Option<String> {
    match &v["isrc"] {
        Value::Array(a) => a.iter().find_map(Value::as_str).map(str::to_string),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

pub(crate) fn song_hit(v: &Value) -> Option<SourceHit> {
    let id = str_of(v, "id")?;
    Some(SourceHit {
        id: format!("track:{id}"),
        title: v["title"].as_str()?.to_string(),
        creator: v["artist"].as_str().map(str::to_string),
        year: v["year"].as_i64().map(|y| y as i32),
        downloads: v["playCount"].as_u64(),
        image: str_of(v, "coverArt").map(|c| cover_path(&c)),
        kind: "track".into(),
        duration_ms: v["duration"].as_u64().map(|d| d * 1000),
        library_artist_id: None,
        isrc: first_isrc(v),
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: Some(track_uri(&id)),
        url: canonical_song(v).map(|u| u.to_url()).unwrap_or_default(),
    })
}

fn album_hit(v: &Value) -> Option<SourceHit> {
    let id = str_of(v, "id")?;
    Some(SourceHit {
        id: format!("album:{id}"),
        title: v["name"].as_str().or(v["title"].as_str())?.to_string(),
        creator: v["artist"].as_str().map(str::to_string),
        year: v["year"].as_i64().map(|y| y as i32),
        downloads: None,
        image: str_of(v, "coverArt").map(|c| cover_path(&c)),
        kind: "album".into(),
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: v["musicBrainzId"]
            .as_str()
            .filter(|m| !m.is_empty())
            .map(|m| format!("https://musicbrainz.org/release/{m}"))
            .unwrap_or_default(),
    })
}

fn source_track(v: &Value, n: u32) -> Option<SourceTrack> {
    let id = str_of(v, "id")?;
    let fmt = match (v["suffix"].as_str(), v["bitRate"].as_u64()) {
        (Some(sfx), Some(br)) => format!("{} {br} kbps (your server)", sfx.to_uppercase()),
        (Some(sfx), None) => format!("{} (your server)", sfx.to_uppercase()),
        _ => "your server".into(),
    };
    Some(SourceTrack {
        title: v["title"].as_str()?.to_string(),
        track_no: v["track"].as_u64().map(|t| t as u32).or(Some(n)),
        duration_ms: v["duration"].as_u64().map(|d| d * 1000),
        format: fmt,
        url: track_uri(&id),
        artist: v["artist"].as_str().map(str::to_string),
    })
}

fn server_name(srv: &Server) -> String {
    match srv.kind.as_deref() {
        Some("navidrome") => "Navidrome".into(),
        Some("gonic") => "Gonic".into(),
        Some("ampache") => "Ampache".into(),
        Some("airsonic") | Some("airsonic-advanced") => "Airsonic".into(),
        Some("lms") => "LMS".into(),
        Some("pubky-music-node") => "Pubky Music node".into(),
        Some(k) if !k.is_empty() => k.to_string(),
        _ => "Your server".into(),
    }
}

pub struct OpenSubsonic;

#[async_trait::async_trait]
impl StreamingService for OpenSubsonic {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play", "playlists"]
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let srv = s.opensubsonic.read().expect("opensubsonic").clone();
        SourceInfo {
            id: SOURCE_ID.into(),
            name: srv
                .as_ref()
                .map(server_name)
                .unwrap_or_else(|| "Your server".into()),
            description: match &srv {
                Some(v) => format!("Your Subsonic server at {}", v.url),
                None => "Navidrome, Airsonic, Gonic, Ampache or any OpenSubsonic server you run".into(),
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
        let a = (limit / 4).max(1).to_string();
        let r = call(
            s,
            &srv,
            "search3",
            &[
                ("query", term),
                ("songCount", &n),
                ("albumCount", &a),
                ("artistCount", "0"),
            ],
        )
        .await?;
        let mut out: Vec<SourceHit> = Vec::new();
        if let Some(l) = r["searchResult3"]["album"].as_array() {
            out.extend(l.iter().filter_map(album_hit));
        }
        if let Some(l) = r["searchResult3"]["song"].as_array() {
            out.extend(l.iter().filter_map(song_hit));
        }
        Ok(out.into_iter().take(limit as usize).collect())
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let srv = server(s)?;
        let (kind, raw) = id.split_once(':').unwrap_or(("track", id));
        let raw = valid_id(raw)?;
        match kind {
            "album" => {
                let r = call(s, &srv, "getAlbum", &[("id", raw)]).await?;
                let a = &r["album"];
                let tracks = a["song"]
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
                    creator: a["artist"].as_str().map(str::to_string),
                    year: a["year"].as_i64().map(|y| y as i32),
                    description: None,
                    licence: None,
                    image: str_of(a, "coverArt").map(|c| cover_path(&c)),
                    url: String::new(),
                    tracks,
                    library_album_id: None,
                })
            }
            "track" => {
                let r = call(s, &srv, "getSong", &[("id", raw)]).await?;
                let t = &r["song"];
                Ok(SourceItem {
                    source: SOURCE_ID.into(),
                    id: format!("track:{raw}"),
                    title: t["album"]
                        .as_str()
                        .or(t["title"].as_str())
                        .unwrap_or("Untitled")
                        .to_string(),
                    creator: t["artist"].as_str().map(str::to_string),
                    year: t["year"].as_i64().map(|y| y as i32),
                    description: None,
                    licence: None,
                    image: str_of(t, "coverArt").map(|c| cover_path(&c)),
                    url: canonical_song(t).map(|u| u.to_url()).unwrap_or_default(),
                    tracks: source_track(t, 1).into_iter().collect(),
                    library_album_id: None,
                })
            }
            _ => Err(ApiError::bad_request("server items are track:<id> or album:<id>")),
        }
    }
}

// ---- playback ----

/// Open `subsonic:track:<id>` for a zone: a freshly signed stream URL (original file, no transcoding).
pub(crate) fn open_track_blocking(
    shared: &Shared,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: track_uri(id),
        reason,
    };
    valid_id(id).map_err(|e| fail(e.message))?;
    let srv = shared
        .read()
        .map_err(|_| fail("server settings".into()))?
        .clone()
        .ok_or_else(|| fail("no Subsonic server is connected".into()))?;
    let url = signed_url(&srv, "stream", &[("id", id), ("format", "raw")]).map_err(fail)?;
    // An error names the URL: drop its signed query
    let signed = url.query().map(str::to_string).unwrap_or_default();
    crate::sources::open_http_stream(url.as_str()).map_err(|e| match e {
        pm_zones::SourceError::Open { reason, .. } => fail(reason.replace(&signed, "<signed>")),
        other => other,
    })
}

/// Relay a method's bytes (stream, cover) so the signed URL never reaches the page.
async fn relay(
    s: &NodeState,
    method: &str,
    params: &[(&str, &str)],
    fallback_type: &str,
) -> Result<axum::response::Response, ApiError> {
    let srv = server(s)?;
    let url = signed_url(&srv, method, params).map_err(upstream)?;
    let resp = s
        .http_client()
        .get(url)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream(format!("{method} answered {}", resp.status())));
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(fallback_type)
        .to_string();
    if ctype.starts_with("application/json") || ctype.starts_with("text/xml") {
        return Err(ApiError::not_found(format!(
            "the server has no {method} for this id"
        )));
    }
    let mut out = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, ctype)
        .header(axum::http::header::CACHE_CONTROL, "private, max-age=3600");
    if let Some(n) = resp.content_length() {
        out = out.header(axum::http::header::CONTENT_LENGTH, n);
    }
    out.body(axum::body::Body::from_stream(resp.bytes_stream()))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// `GET /v1/sources/subsonic/stream/{id}`: a song for browsers (MP3 320 when the server
/// transcodes, else the original).
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let id = valid_id(&id)?.to_string();
    let mp3 = [("id", id.as_str()), ("format", "mp3"), ("maxBitRate", "320")];
    match relay(&s, "stream", &mp3, "audio/mpeg").await {
        // A server that cannot transcode (no ffmpeg) refuses MP3: the original file then
        Err(e) if e.status == StatusCode::NOT_FOUND => {
            relay(
                &s,
                "stream",
                &[("id", id.as_str()), ("format", "raw")],
                "audio/flac",
            )
            .await
        }
        other => other,
    }
}

/// `GET /v1/sources/subsonic/cover/{id}`.
pub(crate) async fn cover(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let id = valid_id(&id)?.to_string();
    relay(&s, "getCoverArt", &[("id", &id), ("size", "600")], "image/jpeg").await
}

// ---- connecting the server ----

fn view(srv: Option<&Server>) -> SubsonicServerView {
    SubsonicServerView {
        connected: srv.is_some(),
        url: srv.map(|v| v.url.clone()),
        user: srv.map(|v| v.user.clone()),
        name: srv.map(server_name),
        version: srv.and_then(|v| v.version.clone()),
    }
}

/// `GET /v1/sources/subsonic/server`: the connected server, never the password.
pub(crate) async fn get_server(State(s): State<Arc<NodeState>>) -> ApiResult<SubsonicServerView> {
    Ok(Json(view(s.opensubsonic.read().expect("opensubsonic").as_ref())))
}

/// `PUT /v1/sources/subsonic/server {url, user, password}`: checked with `ping` before it is kept.
pub(crate) async fn set_server(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<SubsonicServerForm>,
) -> ApiResult<SubsonicServerView> {
    let url = body
        .url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches("/rest")
        .to_string();
    let parsed = url::Url::parse(&url)
        .map_err(|_| ApiError::bad_request("the server address must be an http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("the server address must be an http(s) URL"));
    }
    if body.user.trim().is_empty() || body.password.is_empty() {
        return Err(ApiError::bad_request("user and password are required"));
    }
    let mut srv = Server {
        url,
        user: body.user.trim().to_string(),
        password: body.password,
        kind: None,
        version: None,
    };
    let r = call(&s, &srv, "ping", &[]).await?;
    srv.kind = r["type"].as_str().map(str::to_string);
    srv.version = r["serverVersion"]
        .as_str()
        .or(r["version"].as_str())
        .map(str::to_string);
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(&srv).expect("serializes"))?;
    *s.opensubsonic.write().expect("opensubsonic") = Some(srv.clone());
    tracing::info!(url = %srv.url, kind = ?srv.kind, "Subsonic server connected");
    Ok(Json(view(Some(&srv))))
}

/// `DELETE /v1/sources/subsonic/server`.
pub(crate) async fn forget_server(State(s): State<Arc<NodeState>>) -> ApiResult<SubsonicServerView> {
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(SETTING)?;
    *s.opensubsonic.write().expect("opensubsonic") = None;
    Ok(Json(view(None)))
}

/// `GET /v1/sources/subsonic/playlists`: the server's playlists.
pub(crate) async fn playlists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SubsonicPlaylist>> {
    let srv = server(&s)?;
    let r = call(&s, &srv, "getPlaylists", &[]).await?;
    Ok(Json(
        r["playlists"]["playlist"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|p| {
                        Some(SubsonicPlaylist {
                            id: str_of(p, "id")?,
                            name: p["name"].as_str().unwrap_or("Playlist").to_string(),
                            songs: p["songCount"].as_u64().unwrap_or(0) as usize,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// `POST /v1/sources/subsonic/playlists/{id}/import`: a private Pubky playlist of the songs the
/// server can name (MusicBrainz id or ISRC); the rest are counted as skipped.
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<crate::tidal::ImportReport> {
    let srv = server(&s)?;
    let id = valid_id(&id)?.to_string();
    let r = call(&s, &srv, "getPlaylist", &[("id", &id)]).await?;
    let p = &r["playlist"];
    let mut uris = Vec::new();
    let mut skipped = 0;
    for e in p["entry"].as_array().cloned().unwrap_or_default() {
        match canonical_song(&e) {
            Some(u) => uris.push(u.to_url()),
            None => skipped += 1,
        }
    }
    if uris.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the playlist's songs has a MusicBrainz id or ISRC on the server: tag them (Picard) and try again",
        ));
    }
    let imported = uris.len();
    let Json(detail) = crate::playlists::create(
        State(s.clone()),
        Json(CreatePlaylist {
            name: p["name"].as_str().unwrap_or("Playlist").to_string(),
            description: p["comment"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_string),
            image: None,
            uris,
            track_ids: Vec::new(),
            sources: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn songs_map_to_canonical_ids_and_never_carry_auth() {
        let srv = Server {
            url: "http://nas:4533/".into(),
            user: "me".into(),
            password: "sesame".into(),
            ..Default::default()
        };
        let u = signed_url(&srv, "stream", &[("id", "ab12")]).unwrap();
        let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
        assert_eq!(u.path(), "/rest/stream");
        assert!(!u.as_str().contains("sesame"));
        let mut h = Md5::new();
        h.update(b"sesame");
        h.update(q["s"].as_bytes());
        assert_eq!(q["t"], hex::encode(h.finalize()));
        assert!(!format!("{srv:?}").contains("sesame"));

        let song = serde_json::json!({"id": "ab12", "title": "Bohemian Rhapsody", "artist": "Queen", "album": "A Night at the Opera",
            "duration": 354, "coverArt": "al-1", "suffix": "flac", "musicBrainzId": "b1a9c0e9-d987-4042-ae91-78d6a3267d69", "isrc": ["GBUM71029604"]});
        let h = song_hit(&song).unwrap();
        assert_eq!(h.uri.as_deref(), Some("subsonic:track:ab12"));
        assert_eq!(
            h.url,
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"
        );
        assert_eq!(h.isrc.as_deref(), Some("GBUM71029604"));
        assert_eq!(h.image.as_deref(), Some("/v1/sources/subsonic/cover/al-1"));
        let isrc_only = serde_json::json!({"id": 7, "title": "x", "isrc": "GBUM71029604"});
        assert_eq!(
            canonical_song(&isrc_only).unwrap().to_url(),
            "https://musicbrainz.org/isrc/GBUM71029604"
        );
        assert!(canonical_song(&serde_json::json!({"id": "1", "title": "untagged"})).is_none());
        assert!(valid_id("../x").is_err());
    }
}
