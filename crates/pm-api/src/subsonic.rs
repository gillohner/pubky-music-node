//! OpenSubsonic (handoff M4): enough of the Subsonic API for existing
//! clients (Symfonium, Feishin, Amperfy, play:Sub, Supersonic) to browse,
//! search, stream and keep playlists. JSON responses (`f=json`); ID3
//! browsing (`getArtists`/`getArtist`/`getAlbum`) is the primary path, with
//! `getMusicDirectory` mapped onto the same ids for older clients.
//!
//! Auth: username from the node config, password = the API token (plain
//! `p=` or `t=`+`s=` token auth). Without a token the node accepts anyone,
//! like `/v1`. Streams are the original files (no transcoding yet), which is
//! what these clients want on a LAN; remote use goes through M5.
//!
//! Ids: songs are library track ids, albums `al-<id>`, artists `ar-<id>`,
//! playlists `pl-<post id>`, cover art `al-<album id>`.

use crate::activity::resolve_described;
use crate::playlists::{self, store};
use crate::state::NodeState;
use crate::types::DescribedTrack;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use md5::{Digest, Md5};
use pm_library::{Library, MusicPostRow};
use pm_model::{Album, MusicUri, Track, TrackId};
use pm_pubky::schemas::post::TYPE_PLAYLIST;
use pm_pubky::{MusicContent, MusicPost, Playlist, PlaylistItem, Visibility};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

const API_VERSION: &str = "1.16.1";

pub fn router(state: Arc<NodeState>) -> Router {
    Router::new().route("/rest/{method}", any(rest)).with_state(state)
}

type Params = HashMap<String, String>;

struct SubErr {
    code: u16,
    message: String,
}

impl SubErr {
    fn new(code: u16, m: impl Into<String>) -> Self {
        Self {
            code,
            message: m.into(),
        }
    }
    fn missing(p: &str) -> Self {
        Self::new(10, format!("Required parameter is missing: {p}"))
    }
    fn not_found(what: impl Into<String>) -> Self {
        Self::new(70, what)
    }
}

impl From<pm_library::LibraryError> for SubErr {
    fn from(e: pm_library::LibraryError) -> Self {
        Self::new(0, e.to_string())
    }
}

impl From<crate::ApiError> for SubErr {
    fn from(e: crate::ApiError) -> Self {
        let code = match e.status {
            StatusCode::NOT_FOUND => 70,
            StatusCode::BAD_REQUEST => 10,
            _ => 0,
        };
        Self::new(code, e.message)
    }
}

enum Reply {
    Json(Map<String, Value>),
    Raw(Response),
}

/// Response format: Subsonic's default is XML; `f=json` (what most modern
/// clients send) selects JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Json,
    Xml,
}

fn fmt_of(p: &Params) -> Fmt {
    match p.get("f").map(String::as_str) {
        Some("json") | Some("jsonp") => Fmt::Json,
        _ => Fmt::Xml,
    }
}

fn envelope(fmt: Fmt, status: &str, mut body: Map<String, Value>) -> Response {
    body.insert("status".into(), json!(status));
    body.insert("version".into(), json!(API_VERSION));
    body.insert("type".into(), json!("pubky-music-node"));
    body.insert("serverVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    body.insert("openSubsonic".into(), json!(true));
    match fmt {
        Fmt::Json => {
            let doc = json!({ "subsonic-response": Value::Object(body) });
            (
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                doc.to_string(),
            )
                .into_response()
        }
        Fmt::Xml => (
            [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
            to_xml(&body),
        )
            .into_response(),
    }
}

fn error_response(fmt: Fmt, e: SubErr) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), json!({ "code": e.code, "message": e.message }));
    envelope(fmt, "failed", m)
}

// ---- XML rendering of the same shapes ----

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' && c != '\r' => {}
            c => o.push(c),
        }
    }
    o
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Subsonic XML convention: scalars become attributes, arrays become
/// repeated child elements, objects become child elements. A `value`
/// string inside an object becomes the element's text.
fn xml_element(out: &mut String, name: &str, v: &Value) {
    match v {
        Value::Object(m) => {
            out.push('<');
            out.push_str(name);
            let mut children: Vec<(&String, &Value)> = Vec::new();
            let mut text: Option<String> = None;
            for (k, val) in m {
                match val {
                    Value::Array(_) | Value::Object(_) => children.push((k, val)),
                    Value::Null => {}
                    _ if k == "value" => text = scalar_text(val),
                    _ => {
                        if let Some(t) = scalar_text(val) {
                            out.push(' ');
                            out.push_str(k);
                            out.push_str("=\"");
                            out.push_str(&xml_escape(&t));
                            out.push('"');
                        }
                    }
                }
            }
            if children.is_empty() && text.is_none() {
                out.push_str("/>");
                return;
            }
            out.push('>');
            if let Some(t) = text {
                out.push_str(&xml_escape(&t));
            }
            for (k, val) in children {
                xml_element(out, k, val);
            }
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
        Value::Array(items) => {
            for it in items {
                xml_element(out, name, it);
            }
        }
        other => {
            if let Some(t) = scalar_text(other) {
                out.push('<');
                out.push_str(name);
                out.push('>');
                out.push_str(&xml_escape(&t));
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
        }
    }
}

fn to_xml(body: &Map<String, Value>) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    let mut root = body.clone();
    root.insert("xmlns".into(), json!("http://subsonic.org/restapi"));
    xml_element(&mut out, "subsonic-response", &Value::Object(root));
    out
}

async fn rest(
    State(s): State<Arc<NodeState>>,
    Path(method): Path<String>,
    Query(query): Query<Params>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let mut params = query;
    let is_form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"));
    if is_form && !body.is_empty() {
        for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
            params.entry(k.into_owned()).or_insert(v.into_owned());
        }
    }
    let method = method.trim_end_matches(".view").to_string();
    let fmt = fmt_of(&params);
    if let Err(e) = authenticate(&s, &params) {
        return error_response(fmt, e);
    }
    match dispatch(&s, &method, &params, &headers).await {
        Ok(Reply::Json(m)) => envelope(fmt, "ok", m),
        Ok(Reply::Raw(r)) => r,
        Err(e) => error_response(fmt, e),
    }
}

fn authenticate(s: &NodeState, p: &Params) -> Result<(), SubErr> {
    let Some(token) = &s.token else {
        return Ok(());
    };
    let user = p.get("u").map(String::as_str).unwrap_or("");
    if user != s.subsonic_user {
        return Err(SubErr::new(40, "Wrong username or password"));
    }
    if let (Some(t), Some(salt)) = (p.get("t"), p.get("s")) {
        let mut h = Md5::new();
        h.update(token.as_bytes());
        h.update(salt.as_bytes());
        let want = hex::encode(h.finalize());
        if want.eq_ignore_ascii_case(t) {
            return Ok(());
        }
        return Err(SubErr::new(40, "Wrong username or password"));
    }
    if let Some(pw) = p.get("p") {
        let plain = match pw.strip_prefix("enc:") {
            Some(h) => hex::decode(h)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or_default(),
            None => pw.clone(),
        };
        if plain == *token {
            return Ok(());
        }
    }
    Err(SubErr::new(40, "Wrong username or password"))
}

fn want(p: &Params, k: &str) -> Result<String, SubErr> {
    p.get(k)
        .cloned()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| SubErr::missing(k))
}

fn num(p: &Params, k: &str, default: u32) -> u32 {
    p.get(k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

// ---- JSON shapes ----

fn iso(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

fn song_json(t: &Track) -> Value {
    let id = t.id.map(|i| i.0).unwrap_or(0);
    let album_id = t.album.as_ref().and_then(|a| a.id);
    let secs = t.duration_ms.unwrap_or(0) / 1000;
    let suffix = t
        .file
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let (mime, _) = mime_for(&t.file.container);
    let bitrate = (t.file.size_bytes * 8).checked_div(secs).unwrap_or(0) / 1000;
    let mut m = json!({
        "id": id.to_string(),
        "isDir": false,
        "title": t.title,
        "album": t.album.as_ref().map(|a| a.title.clone()),
        "artist": t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "),
        "track": t.track_no,
        "discNumber": t.disc_no,
        "year": t.album.as_ref().and_then(|a| a.year),
        "size": t.file.size_bytes,
        "contentType": mime,
        "suffix": suffix,
        "duration": secs,
        "bitRate": bitrate,
        "bitDepth": t.file.format.bits,
        "samplingRate": t.file.format.rate_hz,
        "channelCount": t.file.format.channels,
        "path": t.file.path.to_string_lossy(),
        "created": iso(t.file.mtime_unix),
        "type": "music",
        "mediaType": "song",
        "isVideo": false,
        "musicBrainzId": t.recording_mbid.map(|m| m.to_string()),
    });
    if let Some(aid) = album_id {
        m["albumId"] = json!(format!("al-{aid}"));
        m["parent"] = json!(format!("al-{aid}"));
        m["coverArt"] = json!(format!("al-{aid}"));
    }
    if let Some(a) = t.artists.first().and_then(|a| a.id) {
        m["artistId"] = json!(format!("ar-{a}"));
    }
    m
}

fn album_json(lib: &Library, a: &Album) -> Result<Value, SubErr> {
    let id = a.id.unwrap_or(0);
    let tracks = lib.album_tracks(id)?;
    let duration: u64 = tracks.iter().filter_map(|t| t.duration_ms).sum::<u64>() / 1000;
    let artist_id = tracks.iter().find_map(|t| t.artists.first().and_then(|ar| ar.id));
    let created = tracks.iter().map(|t| t.file.mtime_unix).max().unwrap_or(0);
    let mut m = json!({
        "id": format!("al-{id}"),
        "name": a.title,
        "title": a.title,
        "album": a.title,
        "artist": a.album_artist.clone().unwrap_or_else(|| "Various artists".into()),
        "year": a.year,
        "songCount": tracks.len(),
        "duration": duration,
        "created": iso(created),
        "isDir": true,
        "musicBrainzId": a.release_mbid.map(|m| m.to_string()),
    });
    if lib.album_art(id)?.is_some() {
        m["coverArt"] = json!(format!("al-{id}"));
    }
    if let Some(ar) = artist_id {
        m["artistId"] = json!(format!("ar-{ar}"));
        m["parent"] = json!(format!("ar-{ar}"));
    }
    Ok(m)
}

fn artist_json(a: &pm_library::ArtistSummary) -> Value {
    json!({
        "id": format!("ar-{}", a.id),
        "name": a.name,
        "albumCount": a.albums,
        "musicBrainzId": a.mbid.map(|m| m.to_string()),
    })
}

fn mime_for(container: &str) -> (&'static str, &'static str) {
    match container {
        "flac" => ("audio/flac", "flac"),
        "wav" => ("audio/wav", "wav"),
        "aiff" => ("audio/aiff", "aiff"),
        "alac" | "isomp4" | "mp4" | "m4a" => ("audio/mp4", "m4a"),
        "dsf" => ("audio/x-dsf", "dsf"),
        "dff" => ("audio/x-dff", "dff"),
        "mp3" => ("audio/mpeg", "mp3"),
        "aac" => ("audio/aac", "m4a"),
        "vorbis" => ("audio/ogg", "ogg"),
        _ => ("application/octet-stream", "bin"),
    }
}

fn parse_id(id: &str, prefix: &str) -> Option<i64> {
    id.strip_prefix(prefix).and_then(|n| n.parse().ok())
}

// ---- playlists as Subsonic sees them ----

fn playlist_rows(lib: &Library) -> Result<Vec<(MusicPostRow, Playlist)>, SubErr> {
    let mut out = Vec::new();
    for row in lib.list_music_posts(Some(TYPE_PLAYLIST))? {
        let Ok(post) = serde_json::from_str::<pubky_app_specs::PubkyAppPost>(&row.json) else {
            continue;
        };
        if let Ok(Some(m)) = MusicPost::from_post(post, Some(&row.id)) {
            if let Some(p) = m.as_playlist() {
                out.push((row, p.clone()));
            }
        }
    }
    Ok(out)
}

fn playlist_tracks(lib: &Library, p: &Playlist) -> Vec<Track> {
    p.items
        .iter()
        .filter_map(|it| {
            MusicUri::parse(&it.uri)
                .ok()
                .and_then(|m| lib.tracks_by_canonical(&m).ok())
                .and_then(|v| v.into_iter().next())
        })
        .collect()
}

fn playlist_json(lib: &Library, row: &MusicPostRow, p: &Playlist, with_entries: bool) -> Value {
    let tracks = playlist_tracks(lib, p);
    let duration: u64 = tracks.iter().filter_map(|t| t.duration_ms).sum::<u64>() / 1000;
    let mut m = json!({
        "id": format!("pl-{}", row.id),
        "name": p.name,
        "comment": p.description,
        "songCount": tracks.len(),
        "duration": duration,
        "public": false,
        "owner": "pubky",
        "created": iso(p.created_at / 1_000_000),
        "changed": iso(p.updated_at / 1_000_000),
    });
    if with_entries {
        m["entry"] = Value::Array(tracks.iter().map(song_json).collect());
    }
    m
}

fn load_playlist(lib: &Library, id: &str) -> Result<(MusicPostRow, Playlist), SubErr> {
    let pid = id.strip_prefix("pl-").unwrap_or(id);
    playlist_rows(lib)?
        .into_iter()
        .find(|(r, _)| r.id == pid)
        .ok_or_else(|| SubErr::not_found("Playlist not found"))
}

fn items_for_song_ids(lib: &Library, ids: &[String]) -> Result<Vec<PlaylistItem>, SubErr> {
    let mut items = Vec::new();
    for sid in ids {
        let id: i64 = sid
            .parse()
            .map_err(|_| SubErr::new(10, format!("bad song id {sid}")))?;
        let t = lib
            .get_track(TrackId(id))?
            .ok_or_else(|| SubErr::not_found(format!("Song {sid} not found")))?;
        match t.publishable_uri() {
            Some(u) => items.push(PlaylistItem::new(u)),
            None => {
                return Err(SubErr::new(
                    70,
                    format!(
                        "“{}” has no MusicBrainz id or ISRC yet, so it cannot go into a playlist",
                        t.title
                    ),
                ))
            }
        }
    }
    Ok(items)
}

fn save_playlist(lib: &Library, id: &str, mut p: Playlist, visibility: Visibility) -> Result<(), SubErr> {
    p.touch();
    let post = MusicPost::new(MusicContent::Playlist(p));
    post.validate(Some(id))
        .map_err(|e| SubErr::new(0, e.to_string()))?;
    store(lib, &post, id, visibility)?;
    Ok(())
}

fn multi(p: &Params, key: &str) -> Vec<String> {
    // axum's Query keeps the last duplicate; clients that send several
    // `songId=` also work with a comma list, which we accept too.
    p.get(key)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ---- dispatch ----

async fn dispatch(
    s: &Arc<NodeState>,
    method: &str,
    p: &Params,
    headers: &HeaderMap,
) -> Result<Reply, SubErr> {
    let mut m = Map::new();
    match method {
        "ping" => {}
        "getLicense" => {
            m.insert(
                "license".into(),
                json!({"valid": true, "email": "", "licenseExpires": "2099-01-01T00:00:00Z"}),
            );
        }
        "getOpenSubsonicExtensions" => {
            m.insert(
                "openSubsonicExtensions".into(),
                json!([{"name": "formPost", "versions": [1]}, {"name": "songLyrics", "versions": [1]}]),
            );
        }
        "getMusicFolders" => {
            m.insert(
                "musicFolders".into(),
                json!({"musicFolder": [{"id": 1, "name": "Library"}]}),
            );
        }
        "getGenres" => {
            m.insert("genres".into(), json!({"genre": []}));
        }
        "getUser" => {
            m.insert(
                "user".into(),
                json!({"username": s.subsonic_user, "scrobblingEnabled": true, "adminRole": false, "settingsRole": false,
                       "downloadRole": true, "uploadRole": false, "playlistRole": true, "coverArtRole": true,
                       "commentRole": false, "podcastRole": false, "streamRole": true, "jukeboxRole": false,
                       "shareRole": false, "videoConversionRole": false, "folder": [1]}),
            );
        }
        "getArtists" | "getIndexes" => {
            let lib = s.library.lock().expect("library poisoned");
            let artists = lib.list_artists()?;
            let mut buckets: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
            for a in &artists {
                let first = a
                    .name
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "#".into());
                let key = if first.chars().all(|c| c.is_alphabetic()) {
                    first
                } else {
                    "#".into()
                };
                buckets.entry(key).or_default().push(artist_json(a));
            }
            let index: Vec<Value> = buckets
                .into_iter()
                .map(|(name, artist)| json!({"name": name, "artist": artist}))
                .collect();
            let key = if method == "getArtists" {
                "artists"
            } else {
                "indexes"
            };
            m.insert(
                key.into(),
                json!({"ignoredArticles": "", "index": index, "lastModified": 0}),
            );
        }
        "getArtist" => {
            let id = parse_id(&want(p, "id")?, "ar-").ok_or_else(|| SubErr::not_found("Artist not found"))?;
            let lib = s.library.lock().expect("library poisoned");
            let a = lib
                .get_artist(id)?
                .ok_or_else(|| SubErr::not_found("Artist not found"))?;
            let albums = lib
                .artist_albums(id)?
                .iter()
                .map(|al| album_json(&lib, al))
                .collect::<Result<Vec<_>, _>>()?;
            let mut aj = artist_json(&a);
            aj["album"] = Value::Array(albums);
            m.insert("artist".into(), aj);
        }
        "getArtistInfo2" | "getArtistInfo" => {
            let key = if method == "getArtistInfo2" {
                "artistInfo2"
            } else {
                "artistInfo"
            };
            m.insert(key.into(), json!({}));
        }
        "getAlbumInfo2" | "getAlbumInfo" => {
            let key = if method == "getAlbumInfo2" {
                "albumInfo2"
            } else {
                "albumInfo"
            };
            m.insert(key.into(), json!({}));
        }
        "getAlbum" => {
            let id = parse_id(&want(p, "id")?, "al-").ok_or_else(|| SubErr::not_found("Album not found"))?;
            let lib = s.library.lock().expect("library poisoned");
            let a = lib
                .get_album(id)?
                .ok_or_else(|| SubErr::not_found("Album not found"))?;
            let mut aj = album_json(&lib, &a)?;
            aj["song"] = Value::Array(lib.album_tracks(id)?.iter().map(song_json).collect());
            m.insert("album".into(), aj);
        }
        "getMusicDirectory" => {
            let id = want(p, "id")?;
            let lib = s.library.lock().expect("library poisoned");
            if let Some(aid) = parse_id(&id, "al-") {
                let a = lib
                    .get_album(aid)?
                    .ok_or_else(|| SubErr::not_found("Album not found"))?;
                let children: Vec<Value> = lib.album_tracks(aid)?.iter().map(song_json).collect();
                m.insert(
                    "directory".into(),
                    json!({"id": id, "name": a.title, "child": children}),
                );
            } else if let Some(arid) = parse_id(&id, "ar-") {
                let a = lib
                    .get_artist(arid)?
                    .ok_or_else(|| SubErr::not_found("Artist not found"))?;
                let children = lib
                    .artist_albums(arid)?
                    .iter()
                    .map(|al| album_json(&lib, al))
                    .collect::<Result<Vec<_>, _>>()?;
                m.insert(
                    "directory".into(),
                    json!({"id": id, "name": a.name, "child": children}),
                );
            } else {
                return Err(SubErr::not_found("Directory not found"));
            }
        }
        "getAlbumList2" | "getAlbumList" => {
            let size = num(p, "size", 10).min(500);
            let offset = num(p, "offset", 0);
            let kind = p.get("type").map(String::as_str).unwrap_or("alphabeticalByName");
            let lib = s.library.lock().expect("library poisoned");
            let mut albums = match kind {
                "newest" | "recent" | "frequent" => lib.recent_albums(offset, size)?,
                "random" => {
                    let mut all = lib.list_albums(None, 5000)?;
                    let seed = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos() as usize)
                        .unwrap_or(0);
                    let n = all.len();
                    if n > 0 {
                        all.rotate_left(seed % n);
                    }
                    all.truncate(size as usize);
                    all
                }
                "starred" | "byGenre" => Vec::new(),
                _ => {
                    let mut all = lib.list_albums(None, 5000)?;
                    match kind {
                        "alphabeticalByArtist" => all.sort_by_key(|a| {
                            (
                                a.album_artist.clone().unwrap_or_default().to_lowercase(),
                                a.title.to_lowercase(),
                            )
                        }),
                        "byYear" => all.sort_by_key(|a| (a.year.unwrap_or(0), a.title.to_lowercase())),
                        _ => all.sort_by_key(|a| a.title.to_lowercase()),
                    }
                    all.into_iter()
                        .skip(offset as usize)
                        .take(size as usize)
                        .collect()
                }
            };
            albums.truncate(size as usize);
            let list = albums
                .iter()
                .map(|a| album_json(&lib, a))
                .collect::<Result<Vec<_>, _>>()?;
            let key = if method == "getAlbumList2" {
                "albumList2"
            } else {
                "albumList"
            };
            m.insert(key.into(), json!({"album": list}));
        }
        "getSong" => {
            let id: i64 = want(p, "id")?
                .parse()
                .map_err(|_| SubErr::not_found("Song not found"))?;
            let lib = s.library.lock().expect("library poisoned");
            let t = lib
                .get_track(TrackId(id))?
                .ok_or_else(|| SubErr::not_found("Song not found"))?;
            m.insert("song".into(), song_json(&t));
        }
        "getRandomSongs" => {
            let size = num(p, "size", 10).min(500);
            let lib = s.library.lock().expect("library poisoned");
            let songs: Vec<Value> = lib.random_tracks(size)?.iter().map(song_json).collect();
            m.insert("randomSongs".into(), json!({"song": songs}));
        }
        "search3" | "search2" => {
            let q = p.get("query").cloned().unwrap_or_default();
            let q = q.trim().trim_matches('"').to_string();
            let artist_count = num(p, "artistCount", 20);
            let album_count = num(p, "albumCount", 20);
            let song_count = num(p, "songCount", 20);
            let lib = s.library.lock().expect("library poisoned");
            let ql = q.to_lowercase();
            let artists: Vec<Value> = lib
                .list_artists()?
                .iter()
                .filter(|a| q.is_empty() || a.name.to_lowercase().contains(&ql))
                .take(artist_count as usize)
                .map(artist_json)
                .collect();
            let albums = if q.is_empty() {
                lib.list_albums(None, album_count)?
            } else {
                lib.search_albums(&q, album_count)?
            };
            let albums = albums
                .iter()
                .map(|a| album_json(&lib, a))
                .collect::<Result<Vec<_>, _>>()?;
            let songs: Vec<Value> = if q.is_empty() {
                lib.list_tracks(None, song_count)?
            } else {
                lib.search_tracks(&q, song_count)?
            }
            .iter()
            .map(song_json)
            .collect();
            let key = if method == "search3" {
                "searchResult3"
            } else {
                "searchResult2"
            };
            m.insert(
                key.into(),
                json!({"artist": artists, "album": albums, "song": songs}),
            );
        }
        "getStarred" | "getStarred2" => {
            let key = if method == "getStarred2" {
                "starred2"
            } else {
                "starred"
            };
            let lib = s.library.lock().expect("library poisoned");
            let f = crate::favourites::collect(&lib).map_err(|e| SubErr::new(0, e.message))?;
            let albums = f
                .albums
                .iter()
                .map(|a| album_json(&lib, a))
                .collect::<Result<Vec<_>, _>>()?;
            m.insert(
                key.into(),
                json!({
                    "artist": f.artists.iter().map(artist_json).collect::<Vec<_>>(),
                    "album": albums,
                    "song": f.tracks.iter().map(song_json).collect::<Vec<_>>(),
                }),
            );
        }
        "star" | "unstar" => {
            let mut refs = Vec::new();
            if let Some(id) = p.get("id").and_then(|v| v.parse::<i64>().ok()) {
                refs.push(crate::types::FavouriteRef::track(id));
            }
            if let Some(id) = p.get("albumId").and_then(|v| parse_id(v, "al-")) {
                refs.push(crate::types::FavouriteRef::album(id));
            }
            if let Some(id) = p.get("artistId").and_then(|v| parse_id(v, "ar-")) {
                refs.push(crate::types::FavouriteRef::artist(id));
            }
            if refs.is_empty() {
                return Err(SubErr::new(10, "Required parameter is missing"));
            }
            {
                let lib = s.library.lock().expect("library poisoned");
                for r in &refs {
                    let uri =
                        crate::favourites::canonical_for(&lib, r).map_err(|e| SubErr::new(70, e.message))?;
                    if method == "star" {
                        crate::favourites::add(&lib, uri)?;
                    } else {
                        lib.remove_favourite(&uri.to_url())?;
                    }
                }
            }
            s.notify_library();
        }
        "setRating" | "savePlayQueue" => {}
        "getPlayQueue" => {}
        "getNowPlaying" => {
            m.insert("nowPlaying".into(), json!({"entry": []}));
        }
        "getLyrics" => {
            m.insert("lyrics".into(), json!({}));
        }
        "getLyricsBySongId" => {
            m.insert("lyricsList".into(), json!({"structuredLyrics": []}));
        }
        "getPlaylists" => {
            let lib = s.library.lock().expect("library poisoned");
            let list: Vec<Value> = playlist_rows(&lib)?
                .iter()
                .map(|(r, pl)| playlist_json(&lib, r, pl, false))
                .collect();
            m.insert("playlists".into(), json!({"playlist": list}));
        }
        "getPlaylist" => {
            let lib = s.library.lock().expect("library poisoned");
            let (row, pl) = load_playlist(&lib, &want(p, "id")?)?;
            m.insert("playlist".into(), playlist_json(&lib, &row, &pl, true));
        }
        "createPlaylist" => {
            let lib = s.library.lock().expect("library poisoned");
            let songs = multi(p, "songId");
            let items = items_for_song_ids(&lib, &songs)?;
            if let Some(pid) = p.get("playlistId") {
                // Subsonic semantics: createPlaylist with playlistId replaces the item list.
                let (row, mut pl) = load_playlist(&lib, pid)?;
                pl.items = items;
                if let Some(n) = p.get("name") {
                    pl.name = n.clone();
                }
                save_playlist(&lib, &row.id, pl, Visibility::Private)?;
                let (row, pl) = load_playlist(&lib, pid)?;
                m.insert("playlist".into(), playlist_json(&lib, &row, &pl, true));
            } else {
                let name = want(p, "name")?;
                let pl = Playlist::new(name, None, None, items);
                let post = MusicPost::new(MusicContent::Playlist(pl));
                let id = post.create_id();
                post.validate(Some(&id))
                    .map_err(|e| SubErr::new(0, e.to_string()))?;
                store(&lib, &post, &id, Visibility::Private)?;
                let (row, pl) = load_playlist(&lib, &id)?;
                m.insert("playlist".into(), playlist_json(&lib, &row, &pl, true));
            }
            drop(lib);
            s.notify_library();
        }
        "updatePlaylist" => {
            let lib = s.library.lock().expect("library poisoned");
            let (row, mut pl) = load_playlist(&lib, &want(p, "playlistId")?)?;
            if let Some(n) = p.get("name") {
                pl.name = n.clone();
            }
            if let Some(c) = p.get("comment") {
                pl.description = Some(c.clone()).filter(|c| !c.trim().is_empty());
            }
            let mut remove: Vec<usize> = multi(p, "songIndexToRemove")
                .iter()
                .filter_map(|i| i.parse().ok())
                .collect();
            remove.sort_unstable_by(|a, b| b.cmp(a));
            for i in remove {
                if i < pl.items.len() {
                    pl.items.remove(i);
                }
            }
            pl.items
                .extend(items_for_song_ids(&lib, &multi(p, "songIdToAdd"))?);
            save_playlist(&lib, &row.id, pl, Visibility::Private)?;
            drop(lib);
            s.notify_library();
        }
        "deletePlaylist" => {
            let lib = s.library.lock().expect("library poisoned");
            let (row, _) = load_playlist(&lib, &want(p, "id")?)?;
            lib.delete_music_post(&row.id, playlists::now_micros())?;
            drop(lib);
            s.notify_library();
        }
        "scrobble" => {
            let submission = p.get("submission").map(|v| v != "false").unwrap_or(true);
            if submission {
                let id: i64 = want(p, "id")?
                    .parse()
                    .map_err(|_| SubErr::not_found("Song not found"))?;
                let lib = s.library.lock().expect("library poisoned");
                let t = lib
                    .get_track(TrackId(id))?
                    .ok_or_else(|| SubErr::not_found("Song not found"))?;
                if let Some(uri) = t.publishable_uri() {
                    let d = DescribedTrack {
                        title: t.title.clone(),
                        uri: Some(uri.to_url()),
                        ..Default::default()
                    };
                    if resolve_described(&lib, &d)?.is_some() {
                        let a = pm_pubky::Activity::new(uri, pm_pubky::ActivitySource::Local, t.duration_ms);
                        let post = MusicPost::new(MusicContent::Activity(a));
                        let pid = post.create_id();
                        if post.validate(Some(&pid)).is_ok() {
                            let _ = store(&lib, &post, &pid, Visibility::Private);
                        }
                    }
                }
            }
        }
        "stream" | "download" => {
            let id: i64 = want(p, "id")?
                .parse()
                .map_err(|_| SubErr::not_found("Song not found"))?;
            let t = s
                .library
                .lock()
                .expect("library poisoned")
                .get_track(TrackId(id))?
                .ok_or_else(|| SubErr::not_found("Song not found"))?;
            let bytes = tokio::fs::read(&t.file.path)
                .await
                .map_err(|e| SubErr::not_found(format!("file: {e}")))?;
            let (mime, _) = mime_for(&t.file.container);
            return Ok(Reply::Raw(ranged(headers, bytes, mime)));
        }
        "getCoverArt" => {
            let id = want(p, "id")?;
            let album_id = parse_id(&id, "al-")
                .or_else(|| id.parse().ok())
                .ok_or_else(|| SubErr::not_found("Cover art not found"))?;
            let art = s
                .library
                .lock()
                .expect("library poisoned")
                .album_art(album_id)?
                .ok_or_else(|| SubErr::not_found("Cover art not found"))?;
            let bytes = tokio::fs::read(&art.file)
                .await
                .map_err(|e| SubErr::not_found(format!("cover: {e}")))?;
            return Ok(Reply::Raw(
                (
                    [
                        (header::CONTENT_TYPE, art.mime),
                        (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
                    ],
                    bytes,
                )
                    .into_response(),
            ));
        }
        "startScan" | "getScanStatus" => {
            if method == "startScan" {
                let _ = s.start_scan(s.scan_folders.clone());
            }
            let count = s.library.lock().expect("library poisoned").stats()?.tracks;
            m.insert(
                "scanStatus".into(),
                json!({"scanning": s.is_scanning(), "count": count}),
            );
        }
        other => return Err(SubErr::new(0, format!("Method {other} is not implemented"))),
    }
    Ok(Reply::Json(m))
}

/// Serve `bytes` honouring a single `Range: bytes=a-b` request.
fn ranged(headers: &HeaderMap, bytes: Vec<u8>, mime: &str) -> Response {
    let total = bytes.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            let start: u64 = if a.is_empty() { 0 } else { a.parse().ok()? };
            let end: u64 = if b.is_empty() {
                total.saturating_sub(1)
            } else {
                b.parse().ok()?
            };
            (start <= end && start < total).then_some((start, end.min(total.saturating_sub(1))))
        });
    match range {
        Some((start, end)) => {
            let slice = bytes[start as usize..=end as usize].to_vec();
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, mime.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}")),
                ],
                Body::from(slice),
            )
                .into_response()
        }
        None => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
            ],
            Body::from(bytes),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_follows_the_subsonic_conventions() {
        let mut m = Map::new();
        m.insert(
            "artists".into(),
            json!({"ignoredArticles": "", "index": [
                {"name": "D", "artist": [{"id": "ar-1", "name": "Daft <Punk> & co", "albumCount": 1}]},
                {"name": "N", "artist": [{"id": "ar-2", "name": "New Order", "albumCount": 1, "musicBrainzId": null}]}
            ]}),
        );
        m.insert("status".into(), json!("ok"));
        let xml = to_xml(&m);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?><subsonic-response"));
        assert!(xml.contains("xmlns=\"http://subsonic.org/restapi\""));
        assert!(xml.contains("status=\"ok\""));
        // Attribute order follows the JSON map (alphabetical); check pieces.
        assert!(xml.contains("<artists ignoredArticles=\"\"><index"));
        assert!(xml.contains("name=\"Daft &lt;Punk&gt; &amp; co\""));
        assert!(xml.contains("albumCount=\"1\""));
        assert!(xml.contains("/></index>"));
        assert_eq!(xml.matches("<index ").count(), 2);
        assert_eq!(xml.matches("<artist ").count(), 2);
        assert!(!xml.contains("musicBrainzId"), "nulls are omitted");
        let mut e = Map::new();
        e.insert("error".into(), json!({"code": 40, "message": "Wrong \"creds\""}));
        let xml = to_xml(&e);
        assert!(
            xml.contains("<error code=\"40\" message=\"Wrong &quot;creds&quot;\"/>"),
            "{xml}"
        );
        let mut t = Map::new();
        t.insert("lyrics".into(), json!({"artist": "x", "value": "line one"}));
        assert!(to_xml(&t).contains("<lyrics artist=\"x\">line one</lyrics>"));
    }

    #[test]
    fn format_selection() {
        let mut p = Params::new();
        assert_eq!(fmt_of(&p), Fmt::Xml);
        p.insert("f".into(), "json".into());
        assert_eq!(fmt_of(&p), Fmt::Json);
        p.insert("f".into(), "xml".into());
        assert_eq!(fmt_of(&p), Fmt::Xml);
    }
}
