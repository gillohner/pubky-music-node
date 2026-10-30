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
use crate::sources::NodeSources;
use crate::state::NodeState;
use crate::types::DescribedTrack;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use md5::{Digest, Md5};
use pm_library::{Library, MusicPostRow};
use pm_model::{Album, MusicUri, Track, TrackId};
use pm_pubky::schemas::post::TYPE_PLAYLIST;
use pm_pubky::{MusicContent, MusicPost, Playlist, PlaylistItem, Visibility};
use pm_zones::{Insert, QueueItem, ZoneId};
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
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Last value wins in the map (what clients expect for `v`, `c`, `f`); every
    // `id` is also kept in order for calls that take several (jukebox add).
    let mut params = Params::new();
    let mut ids = Vec::new();
    for (k, v) in url::form_urlencoded::parse(raw_query.as_deref().unwrap_or("").as_bytes()) {
        if k == "id" {
            ids.push(v.to_string());
        }
        params.insert(k.into_owned(), v.into_owned());
    }
    let is_form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"));
    if is_form && !body.is_empty() {
        for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
            if k == "id" {
                ids.push(v.to_string());
            }
            params.entry(k.into_owned()).or_insert(v.into_owned());
        }
    }
    let method = method.trim_end_matches(".view").to_string();
    let fmt = fmt_of(&params);
    if let Err(e) = authenticate(&s, &params) {
        return error_response(fmt, e);
    }
    match dispatch(&s, &method, &params, &ids, &headers).await {
        Ok(Reply::Json(m)) => envelope(fmt, "ok", m),
        Ok(Reply::Raw(r)) => r,
        Err(e) => error_response(fmt, e),
    }
}

fn authenticate(s: &NodeState, p: &Params) -> Result<(), SubErr> {
    let Some(token) = s.token() else {
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
    // OpenSubsonic: ISRCs as a list
    if let Some(i) = &t.isrc {
        m["isrc"] = json!([i.to_string()]);
    }
    if let Some(aid) = album_id {
        m["albumId"] = json!(format!("al-{aid}"));
        m["parent"] = json!(format!("al-{aid}"));
        m["coverArt"] = json!(format!("al-{aid}"));
    }
    if let Some(a) = t.artists.first().and_then(|a| a.id) {
        m["artistId"] = json!(format!("ar-{a}"));
    }
    if let Some(g) = t.genres.first() {
        m["genre"] = json!(g);
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

pub(crate) fn mime_for(container: &str) -> (&'static str, &'static str) {
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

/// Where a Subsonic playlist entry comes from: the synced item at that index, or the node-only
/// song at that index (after the items). Clients remove entries by their position in this list.
#[derive(Clone, Copy)]
enum Entry {
    Item(usize),
    Local(usize),
}

fn playlist_entries(lib: &Library, row_id: &str, p: &Playlist) -> Vec<(Entry, Track)> {
    let mut out: Vec<(Entry, Track)> = p
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            MusicUri::parse(&it.uri)
                .ok()
                .and_then(|m| lib.tracks_by_canonical(&m).ok())
                .and_then(|v| v.into_iter().next())
                .map(|t| (Entry::Item(i), t))
        })
        .collect();
    for (k, n) in playlists::local_ids(lib, row_id).into_iter().enumerate() {
        if let Ok(Some(t)) = lib.get_track(TrackId(n)) {
            out.push((Entry::Local(k), t));
        }
    }
    out
}

fn playlist_json(lib: &Library, row: &MusicPostRow, p: &Playlist, with_entries: bool) -> Value {
    let tracks: Vec<Track> = playlist_entries(lib, &row.id, p)
        .into_iter()
        .map(|(_, t)| t)
        .collect();
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

/// Songs as playlist items, and the ones not identified yet (kept on this node only).
fn items_for_song_ids(lib: &Library, ids: &[String]) -> Result<(Vec<PlaylistItem>, Vec<i64>), SubErr> {
    let mut nums = Vec::with_capacity(ids.len());
    for sid in ids {
        let id: i64 = sid
            .parse()
            .map_err(|_| SubErr::new(10, format!("bad song id {sid}")))?;
        if lib.get_track(TrackId(id))?.is_none() {
            return Err(SubErr::not_found(format!("Song {sid} not found")));
        }
        nums.push(id);
    }
    playlists::split_tracks(lib, &nums).map_err(|e| SubErr::new(0, e.message))
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

/// The zone a Subsonic client's jukebox drives: `?zone=` picks (and
/// remembers) one; otherwise the remembered one, else the first by name.
/// Settings key holding the zone id Subsonic jukebox calls drive.
pub(crate) const JUKEBOX_ZONE_KEY: &str = "subsonic.jukebox_zone";

fn jukebox_zone(s: &NodeState, p: &Params) -> Result<Arc<pm_zones::Zone>, SubErr> {
    const KEY: &str = JUKEBOX_ZONE_KEY;
    let lib = s.library.lock().expect("library poisoned");
    if let Some(z) = p.get("zone").filter(|z| !z.is_empty()) {
        let zone = s
            .zones
            .get(&ZoneId::new(z))
            .ok_or_else(|| SubErr::new(70, format!("no zone {z}")))?;
        lib.set_setting(KEY, z)
            .map_err(|e| SubErr::new(0, e.to_string()))?;
        return Ok(zone);
    }
    if let Some(z) = lib.get_setting(KEY).map_err(|e| SubErr::new(0, e.to_string()))? {
        if let Some(zone) = s.zones.get(&ZoneId::new(&z)) {
            return Ok(zone);
        }
    }
    let mut zones = s.zones.list();
    zones.sort_by(|a, b| a.name.cmp(&b.name));
    zones
        .first()
        .and_then(|z| s.zones.get(&z.id))
        .ok_or_else(|| SubErr::new(0, "the node has no zones"))
}

fn jukebox_status(zone: &pm_zones::Zone) -> Value {
    let st = zone.state();
    let mut v = json!({
        "currentIndex": st.queue_index.map(|i| i as i64).unwrap_or(-1),
        "playing": st.transport == pm_zones::Transport::Playing,
        "gain": 1.0,
    });
    if let Some(np) = &st.now_playing {
        v["position"] = json!(np.position_ms / 1000);
    }
    v
}

async fn dispatch(
    s: &Arc<NodeState>,
    method: &str,
    p: &Params,
    ids: &[String],
    headers: &HeaderMap,
) -> Result<Reply, SubErr> {
    let mut m = Map::new();
    match method {
        // Jukebox: a Subsonic client driving one of the node's zones.
        "jukeboxControl" => {
            let action = want(p, "action")?;
            let zone = jukebox_zone(s, p)?;
            let mut playlist = false;
            match action.as_str() {
                "get" => playlist = true,
                "status" => {}
                "set" | "add" => {
                    let items: Vec<QueueItem> = {
                        let lib = s.library.lock().expect("library poisoned");
                        let mut items = Vec::new();
                        for id in ids {
                            let tid = parse_id(id, "")
                                .ok_or_else(|| SubErr::new(70, format!("unknown song {id}")))?;
                            let t = lib
                                .get_track(TrackId(tid))
                                .map_err(|e| SubErr::new(0, e.to_string()))?
                                .ok_or_else(|| SubErr::new(70, format!("unknown song {id}")))?;
                            if let Some(it) = NodeSources::item_for(&t) {
                                items.push(it);
                            }
                        }
                        items
                    };
                    if action == "set" {
                        zone.stop();
                        zone.clear();
                    }
                    if !items.is_empty() {
                        zone.enqueue(items, Insert::End);
                    }
                }
                "start" => {
                    let st = zone.state();
                    if st.transport == pm_zones::Transport::Stopped && !st.queue.is_empty() {
                        zone.play_index(st.queue_index.unwrap_or(0));
                    } else {
                        zone.play();
                    }
                }
                "stop" => zone.pause(),
                "skip" => {
                    let index = num(p, "index", 0) as usize;
                    zone.play_index(index);
                    let offset = num(p, "offset", 0) as u64;
                    if offset > 0 {
                        let mut events = zone.subscribe();
                        let started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            loop {
                                match events.recv().await {
                                    Ok(pm_zones::ZoneEvent::TrackStarted { .. }) => break true,
                                    Ok(pm_zones::ZoneEvent::Error { .. }) | Err(_) => break false,
                                    Ok(_) => {}
                                }
                            }
                        })
                        .await
                        .unwrap_or(false);
                        if started {
                            zone.seek_ms(offset * 1000);
                        }
                    }
                }
                "remove" => {
                    let index = num(p, "index", 0) as usize;
                    if let Some(it) = zone.queue().get(index) {
                        zone.remove(it.id);
                    }
                }
                "clear" => {
                    zone.stop();
                    zone.clear();
                }
                "shuffle" => {
                    zone.shuffle_upcoming();
                }
                "setGain" => {
                    // 0.0–1.0 → dB on outputs with hardware volume; others keep playing.
                    let gain: f32 = p.get("gain").and_then(|g| g.parse().ok()).unwrap_or(1.0);
                    let db = if gain <= 0.0 {
                        -60.0
                    } else {
                        (20.0 * gain.log10()).clamp(-60.0, 0.0)
                    };
                    let _ = zone.set_volume(pm_model::Volume::Db(db)).await;
                }
                other => return Err(SubErr::new(0, format!("unknown jukebox action {other}"))),
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), zone.wait_acked()).await;
            let mut status = jukebox_status(&zone);
            if playlist {
                let lib = s.library.lock().expect("library poisoned");
                let entries: Vec<Value> = zone
                    .queue()
                    .iter()
                    .filter_map(|it| {
                        it.uri
                            .strip_prefix(crate::sources::TRACK_SCHEME)
                            .and_then(|r| r.parse::<i64>().ok())
                            .and_then(|id| lib.get_track(TrackId(id)).ok().flatten())
                            .map(|t| song_json(&t))
                    })
                    .collect();
                status["entry"] = json!(entries);
                m.insert("jukeboxPlaylist".into(), status);
            } else {
                m.insert("jukeboxStatus".into(), status);
            }
        }
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
            let lib = s.library.lock().expect("library poisoned");
            let list: Vec<Value> = lib
                .list_genres()?
                .iter()
                .map(|g| json!({"value": g.name, "songCount": g.tracks, "albumCount": g.albums}))
                .collect();
            m.insert("genres".into(), json!({"genre": list}));
        }
        "getSongsByGenre" => {
            let name = want(p, "genre")?;
            let count = num(p, "count", 10).min(500);
            let offset = num(p, "offset", 0);
            let lib = s.library.lock().expect("library poisoned");
            let songs: Vec<Value> = match lib.genre_by_name(&name)? {
                Some(g) => lib
                    .genre_tracks(g.id, offset + count)?
                    .iter()
                    .skip(offset as usize)
                    .map(song_json)
                    .collect(),
                None => Vec::new(),
            };
            m.insert("songsByGenre".into(), json!({"song": songs}));
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
        // Both answer in <albumInfo> (Subsonic API; unlike getArtistInfo2's <artistInfo2>).
        "getAlbumInfo2" | "getAlbumInfo" => {
            m.insert("albumInfo".into(), json!({}));
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
                "newest" => lib.recent_albums(offset, size)?,
                "recent" => lib
                    .recently_played_albums(offset + size)?
                    .into_iter()
                    .skip(offset as usize)
                    .collect(),
                "frequent" => lib
                    .most_played_albums(offset + size)?
                    .into_iter()
                    .skip(offset as usize)
                    .collect(),
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
                "starred" => crate::bookmarks::collect(&lib)
                    .map_err(|e| SubErr::new(0, e.message))?
                    .albums
                    .into_iter()
                    .skip(offset as usize)
                    .take(size as usize)
                    .collect(),
                "byGenre" => match p.get("genre").and_then(|g| lib.genre_by_name(g).ok().flatten()) {
                    Some(g) => lib
                        .genre_albums(g.id)?
                        .into_iter()
                        .skip(offset as usize)
                        .take(size as usize)
                        .collect(),
                    None => Vec::new(),
                },
                _ => {
                    let mut all = lib.list_albums(None, 5000)?;
                    match kind {
                        "alphabeticalByArtist" => all.sort_by_key(|a| {
                            (
                                a.album_artist.clone().unwrap_or_default().to_lowercase(),
                                a.title.to_lowercase(),
                            )
                        }),
                        // fromYear > toYear asks for the range in descending order.
                        "byYear" => {
                            let from = num(p, "fromYear", 0) as i32;
                            let to = num(p, "toYear", 9999) as i32;
                            let (lo, hi) = (from.min(to), from.max(to));
                            if p.contains_key("fromYear") || p.contains_key("toYear") {
                                all.retain(|a| a.year.is_some_and(|y| (lo..=hi).contains(&y)));
                            }
                            all.sort_by_key(|a| (a.year.unwrap_or(0), a.title.to_lowercase()));
                            if from > to {
                                all.reverse();
                            }
                        }
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
            // Counts and offsets per kind: clients page through the whole library with an
            // empty query and growing offsets (Music Assistant, Symfonium's sync).
            let artist_count = num(p, "artistCount", 20);
            let album_count = num(p, "albumCount", 20);
            let song_count = num(p, "songCount", 20);
            let artist_offset = num(p, "artistOffset", 0);
            let album_offset = num(p, "albumOffset", 0);
            let song_offset = num(p, "songOffset", 0);
            let lib = s.library.lock().expect("library poisoned");
            let ql = q.to_lowercase();
            let artists: Vec<Value> = lib
                .list_artists()?
                .iter()
                .filter(|a| q.is_empty() || a.name.to_lowercase().contains(&ql))
                .skip(artist_offset as usize)
                .take(artist_count as usize)
                .map(artist_json)
                .collect();
            let albums = match (q.is_empty(), album_count) {
                (_, 0) => Vec::new(),
                (true, _) => lib.recent_albums(album_offset, album_count)?,
                (false, _) => lib
                    .search_albums(&q, album_offset + album_count)?
                    .into_iter()
                    .skip(album_offset as usize)
                    .collect(),
            };
            let albums = albums
                .iter()
                .map(|a| album_json(&lib, a))
                .collect::<Result<Vec<_>, _>>()?;
            let songs: Vec<Value> = match (q.is_empty(), song_count) {
                (_, 0) => Vec::new(),
                (true, _) => lib.tracks_page(song_offset, song_count)?,
                (false, _) => lib
                    .search_tracks(&q, song_offset + song_count)?
                    .into_iter()
                    .skip(song_offset as usize)
                    .collect(),
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
            let f = crate::bookmarks::collect(&lib).map_err(|e| SubErr::new(0, e.message))?;
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
                refs.push(crate::types::ItemRef::track(id));
            }
            if let Some(id) = p.get("albumId").and_then(|v| parse_id(v, "al-")) {
                refs.push(crate::types::ItemRef::album(id));
            }
            if let Some(id) = p.get("artistId").and_then(|v| parse_id(v, "ar-")) {
                refs.push(crate::types::ItemRef::artist(id));
            }
            if refs.is_empty() {
                return Err(SubErr::new(10, "Required parameter is missing"));
            }
            {
                let lib = s.library.lock().expect("library poisoned");
                for r in &refs {
                    crate::bookmarks::set(&lib, r, method == "star")
                        .map_err(|e| SubErr::new(70, e.message))?;
                }
            }
            s.notify_library();
        }
        "setRating" | "savePlayQueue" => {}
        "getPlayQueue" => {}
        "getNowPlaying" => {
            // What the node's zones are playing, when it is a library track.
            let lib = s.library.lock().expect("library poisoned");
            let mut entries = Vec::new();
            for z in s.zones.list() {
                let Some(np) = z.now_playing else { continue };
                let Some(id) = np
                    .item
                    .uri
                    .strip_prefix(crate::sources::TRACK_SCHEME)
                    .and_then(|v| v.parse::<i64>().ok())
                else {
                    continue;
                };
                if let Some(t) = lib.get_track(TrackId(id))? {
                    let mut e = song_json(&t);
                    e["username"] = json!(s.subsonic_user);
                    e["playerName"] = json!(z.name);
                    e["minutesAgo"] = json!(0);
                    entries.push(e);
                }
            }
            m.insert("nowPlaying".into(), json!({"entry": entries}));
        }
        // Legacy lyrics by artist + title: plain text, timestamps stripped.
        "getLyrics" => {
            let lib = s.library.lock().expect("library poisoned");
            let title = p.get("title").cloned().unwrap_or_default();
            let artist = p.get("artist").cloned().filter(|a| !a.is_empty());
            let found = lib
                .find_by_title_artist(&title, artist.as_deref())
                .map_err(|e| SubErr::new(0, e.to_string()))?
                .and_then(|t| t.id.map(|id| (id, t)));
            let mut out = Map::new();
            if let Some((id, t)) = found {
                if let Some((text, _)) = lib.lyrics(id).map_err(|e| SubErr::new(0, e.to_string()))? {
                    let (lines, _) = pm_library::lyrics::parse(&text);
                    let plain: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
                    out.insert(
                        "artist".into(),
                        json!(t.artists.first().map(|a| a.name.clone()).unwrap_or_default()),
                    );
                    out.insert("title".into(), json!(t.title));
                    out.insert("value".into(), json!(plain.join("\n")));
                }
            }
            m.insert("lyrics".into(), Value::Object(out));
        }
        // OpenSubsonic `songLyrics`: structured, timed when the source was.
        "getLyricsBySongId" => {
            let id = want(p, "id")?;
            let tid = parse_id(&id, "").ok_or_else(|| SubErr::new(70, "unknown song"))?;
            let lib = s.library.lock().expect("library poisoned");
            let track = lib
                .get_track(TrackId(tid))
                .map_err(|e| SubErr::new(0, e.to_string()))?
                .ok_or_else(|| SubErr::new(70, "unknown song"))?;
            let mut list = Vec::new();
            if let Some((text, _)) = lib
                .lyrics(TrackId(tid))
                .map_err(|e| SubErr::new(0, e.to_string()))?
            {
                let (lines, synced) = pm_library::lyrics::parse(&text);
                let line: Vec<Value> = lines
                    .iter()
                    .map(|l| match l.ms {
                        Some(ms) if synced => json!({"start": ms, "value": l.text}),
                        _ => json!({"value": l.text}),
                    })
                    .collect();
                list.push(json!({
                    "displayArtist": track.artists.first().map(|a| a.name.clone()).unwrap_or_default(),
                    "displayTitle": track.title,
                    "lang": "xxx",
                    "offset": 0,
                    "synced": synced,
                    "line": line,
                }));
            }
            m.insert("lyricsList".into(), json!({"structuredLyrics": list}));
        }
        // An artist's most played songs (then in album order), by artist name.
        "getTopSongs" => {
            let name = want(p, "artist")?.to_lowercase();
            let count = num(p, "count", 50) as usize;
            let lib = s.library.lock().expect("library poisoned");
            let songs: Vec<Value> = match lib
                .list_artists()?
                .into_iter()
                .find(|a| a.name.to_lowercase() == name)
            {
                Some(a) => {
                    let mut tracks: Vec<(u64, Track)> = lib
                        .artist_tracks(a.id)?
                        .into_iter()
                        .map(|t| (t.id.map(|id| lib.play_count(id).unwrap_or(0)).unwrap_or(0), t))
                        .collect();
                    tracks.sort_by_key(|(plays, _)| std::cmp::Reverse(*plays));
                    tracks.iter().take(count).map(|(_, t)| song_json(t)).collect()
                }
                None => Vec::new(),
            };
            m.insert("topSongs".into(), json!({"song": songs}));
        }
        // Songs by the same artists as the song, album or artist given, in random order
        // (the node has no similarity data of its own yet).
        "getSimilarSongs" | "getSimilarSongs2" => {
            let id = want(p, "id")?;
            let count = num(p, "count", 50) as usize;
            let lib = s.library.lock().expect("library poisoned");
            let (artists, seed): (Vec<i64>, Option<i64>) = if let Some(a) = parse_id(&id, "ar-") {
                (vec![a], None)
            } else if let Some(al) = parse_id(&id, "al-") {
                let mut ids: Vec<i64> = lib
                    .album_tracks(al)?
                    .iter()
                    .flat_map(|t| t.artists.iter().filter_map(|a| a.id))
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                (ids, None)
            } else {
                let tid = id
                    .parse::<i64>()
                    .map_err(|_| SubErr::not_found("Song not found"))?;
                let t = lib
                    .get_track(TrackId(tid))?
                    .ok_or_else(|| SubErr::not_found("Song not found"))?;
                (t.artists.iter().filter_map(|a| a.id).collect(), Some(tid))
            };
            let mut tracks: Vec<Track> = Vec::new();
            for a in artists {
                for t in lib.artist_tracks(a)? {
                    if t.id.map(|i| i.0) != seed && !tracks.iter().any(|x| x.id == t.id) {
                        tracks.push(t);
                    }
                }
            }
            use rand::seq::SliceRandom;
            tracks.shuffle(&mut rand::thread_rng());
            let songs: Vec<Value> = tracks.iter().take(count).map(song_json).collect();
            let key = if method == "getSimilarSongs2" {
                "similarSongs2"
            } else {
                "similarSongs"
            };
            m.insert(key.into(), json!({"song": songs}));
        }
        // No podcasts on this node: empty lists, not errors (clients sync them unasked).
        "getPodcasts" => {
            m.insert("podcasts".into(), json!({"channel": []}));
        }
        "getNewestPodcasts" => {
            m.insert("newestPodcasts".into(), json!({"episode": []}));
        }
        "getPodcastEpisode" => return Err(SubErr::not_found("Podcast episode not found")),
        // Bookmarks resume podcasts and audiobooks, which the node does not have: none kept.
        "getBookmarks" => {
            m.insert("bookmarks".into(), json!({"bookmark": []}));
        }
        "createBookmark" | "deleteBookmark" => {}
        "getPlaylists" => {
            let lib = s.library.lock().expect("library poisoned");
            let list: Vec<Value> = playlist_rows(&lib)?
                .iter()
                .map(|(r, pl)| playlist_json(&lib, r, pl, false))
                .collect();
            m.insert("playlists".into(), json!({"playlist": list}));
        }
        // Internet radio: the node's saved stations.
        "getInternetRadioStations" => {
            let stations = crate::radio::load_saved(s).map_err(|e| SubErr::new(0, e.message))?;
            let list: Vec<Value> = stations
                .iter()
                .map(|st| {
                    json!({"id": st.id, "name": st.name, "streamUrl": st.url, "homePageUrl": st.homepage.clone().unwrap_or_default()})
                })
                .collect();
            m.insert(
                "internetRadioStations".into(),
                json!({"internetRadioStation": list}),
            );
        }
        "createInternetRadioStation" | "updateInternetRadioStation" => {
            let url = want(p, "streamUrl")?;
            let name = want(p, "name")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(SubErr::new(10, "streamUrl must be http(s)"));
            }
            let mut stations = crate::radio::load_saved(s).map_err(|e| SubErr::new(0, e.message))?;
            let existing_id = if method == "updateInternetRadioStation" {
                Some(want(p, "id")?)
            } else {
                None
            };
            let st = crate::radio::Station {
                id: existing_id.clone().unwrap_or_default(),
                name,
                url: url.clone(),
                homepage: p.get("homepageUrl").cloned().filter(|h| !h.is_empty()),
                ..Default::default()
            }
            .with_id();
            if let Some(id) = &existing_id {
                if !stations.iter().any(|x| &x.id == id) {
                    return Err(SubErr::not_found("Station not found"));
                }
                stations.retain(|x| &x.id != id);
            }
            stations.retain(|x| x.id != st.id && x.url != st.url);
            stations.insert(0, st);
            crate::radio::store_saved(s, &stations).map_err(|e| SubErr::new(0, e.message))?;
        }
        "deleteInternetRadioStation" => {
            let id = want(p, "id")?;
            let mut stations = crate::radio::load_saved(s).map_err(|e| SubErr::new(0, e.message))?;
            let before = stations.len();
            stations.retain(|x| x.id != id);
            if stations.len() == before {
                return Err(SubErr::not_found("Station not found"));
            }
            crate::radio::store_saved(s, &stations).map_err(|e| SubErr::new(0, e.message))?;
        }
        "getPlaylist" => {
            let lib = s.library.lock().expect("library poisoned");
            let id = want(p, "id")?;
            let pid = id.strip_prefix("pl-").unwrap_or(&id).to_string();
            let _ = playlists::promote_local(&lib, &pid);
            let (row, pl) = load_playlist(&lib, &id)?;
            m.insert("playlist".into(), playlist_json(&lib, &row, &pl, true));
        }
        "createPlaylist" => {
            let lib = s.library.lock().expect("library poisoned");
            let songs = multi(p, "songId");
            let (items, local) = items_for_song_ids(&lib, &songs)?;
            if let Some(pid) = p.get("playlistId") {
                // Subsonic semantics: createPlaylist with playlistId replaces the item list.
                let (row, mut pl) = load_playlist(&lib, pid)?;
                pl.items = items;
                playlists::set_local_ids(&lib, &row.id, &local).map_err(|e| SubErr::new(0, e.message))?;
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
                playlists::set_local_ids(&lib, &id, &local).map_err(|e| SubErr::new(0, e.message))?;
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
            // Positions are in the list the client was given (items found here, then the
            // node-only songs), not in the stored item list
            let entries = playlist_entries(&lib, &row.id, &pl);
            let mut drop_items: Vec<usize> = Vec::new();
            let mut drop_local: Vec<usize> = Vec::new();
            for i in multi(p, "songIndexToRemove")
                .iter()
                .filter_map(|i| i.parse::<usize>().ok())
            {
                match entries.get(i).map(|(e, _)| *e) {
                    Some(Entry::Item(k)) => drop_items.push(k),
                    Some(Entry::Local(k)) => drop_local.push(k),
                    None => {}
                }
            }
            drop_items.sort_unstable_by(|a, b| b.cmp(a));
            drop_items.dedup();
            for k in drop_items {
                pl.items.remove(k);
            }
            let mut local = playlists::local_ids(&lib, &row.id);
            drop_local.sort_unstable_by(|a, b| b.cmp(a));
            drop_local.dedup();
            for k in drop_local {
                if k < local.len() {
                    local.remove(k);
                }
            }
            let (add, add_local) = items_for_song_ids(&lib, &multi(p, "songIdToAdd"))?;
            pl.items.extend(add);
            local.extend(add_local);
            playlists::set_local_ids(&lib, &row.id, &local).map_err(|e| SubErr::new(0, e.message))?;
            save_playlist(&lib, &row.id, pl, Visibility::Private)?;
            drop(lib);
            s.notify_library();
        }
        "deletePlaylist" => {
            let lib = s.library.lock().expect("library poisoned");
            let (row, _) = load_playlist(&lib, &want(p, "id")?)?;
            lib.delete_music_post(&row.id, playlists::now_micros())?;
            let _ = playlists::set_local_ids(&lib, &row.id, &[]);
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
                // The client played it itself: it goes into the play history like
                // the app's local zone or the browser, under the client's name.
                let client = p.get("c").cloned().unwrap_or_else(|| "subsonic".into());
                let _ = lib.record_play(TrackId(id), Some(&format!("subsonic:{client}")));
                if let Some(uri) = t.publishable_uri() {
                    let d = DescribedTrack {
                        title: t.title.clone(),
                        uri: Some(uri.to_url()),
                        ..Default::default()
                    };
                    if resolve_described(&lib, &d)?.is_some() {
                        // A listen where the history setting says (none when it is off)
                        let _ = crate::listens::record(
                            &lib,
                            uri,
                            pm_pubky::ActivitySource::Local,
                            t.duration_ms,
                            None,
                        );
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
            let (mime, _) = mime_for(&t.file.container);
            let resp = crate::downloads::stream_file(s, id, &t.file, headers, mime, None)
                .await
                .map_err(|e| SubErr::not_found(format!("file: {}", e.message)))?;
            return Ok(Reply::Raw(resp));
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
