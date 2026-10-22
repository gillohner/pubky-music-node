//! Deezer: the public catalogue API (`api.deezer.com`) needs no key for
//! search, albums, artists and 30‑second previews, and exposes ISRC/UPC, so
//! it is the cheapest way to show every release next to the library (Roon's
//! "Versions") before a service subscription exists. Full streams need a
//! Deezer partner agreement and are not attempted.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::ApiError;
use axum::http::StatusCode;
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub const SOURCE_ID: &str = "deezer";
/// Deezer plays previews of this length without a partner agreement.
const PREVIEW_SECS: u64 = 30;

/// Queue URI of a Deezer track: resolved to its preview when played.
pub const TRACK_URI_PREFIX: &str = "deezer:track:";
pub const DEFAULT_BASE: &str = "https://api.deezer.com";

/// The API origin, shared with the audio thread (tests point it at a mock).
pub type SharedBase = Arc<RwLock<String>>;

pub fn shared_base() -> SharedBase {
    Arc::new(RwLock::new(DEFAULT_BASE.to_string()))
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("deezer: {e}"))
}

fn base(s: &NodeState) -> String {
    s.deezer_base
        .read()
        .expect("deezer base")
        .trim_end_matches('/')
        .to_string()
}

pub fn track_uri(id: &str) -> String {
    format!("{TRACK_URI_PREFIX}{id}")
}

fn page_url(kind: &str, id: &str) -> String {
    format!("https://www.deezer.com/{kind}/{id}")
}

fn numeric(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request("bad Deezer id"));
    }
    Ok(id)
}

/// Deezer answers 200 with `{"error": {...}}` for missing things.
fn check(v: Value, what: &str) -> Result<Value, ApiError> {
    if let Some(e) = v.get("error") {
        let code = e["code"].as_u64().unwrap_or(0);
        let msg = e["message"].as_str().unwrap_or("error");
        return if code == 800 || msg.contains("no data") {
            Err(ApiError::not_found(format!("no Deezer {what}")))
        } else {
            Err(upstream(format!("{what}: {msg}")))
        };
    }
    Ok(v)
}

async fn get_json(s: &NodeState, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
    let resp = s
        .http_client()
        .get(format!("{}{path}", base(s)))
        .query(query)
        .send()
        .await
        .map_err(upstream)?;
    if !resp.status().is_success() {
        return Err(upstream(format!("{path} answered {}", resp.status())));
    }
    resp.json().await.map_err(upstream)
}

fn id_of(v: &Value) -> Option<String> {
    v["id"]
        .as_u64()
        .map(|i| i.to_string())
        .or_else(|| v["id"].as_str().map(str::to_string))
}

fn year_of(v: &Value) -> Option<i32> {
    v["release_date"]
        .as_str()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok())
}

fn cover(v: &Value) -> Option<String> {
    v["cover_big"]
        .as_str()
        .or(v["cover_medium"].as_str())
        .or(v["album"]["cover_big"].as_str())
        .or(v["album"]["cover_medium"].as_str())
        .or(v["picture_big"].as_str())
        .or(v["picture_medium"].as_str())
        .map(str::to_string)
}

/// An artist's top tracks on Deezer (30 s previews here), as hits.
pub(crate) async fn artist_top_tracks(
    s: &NodeState,
    artist_id: &str,
    limit: usize,
) -> Result<Vec<SourceHit>, ApiError> {
    let a = numeric(artist_id)?;
    let v = get_json(s, &format!("/artist/{a}/top"), &[("limit", limit.to_string())]).await?;
    Ok(v["data"]
        .as_array()
        .map(|l| l.iter().filter_map(|x| hit("track", x)).collect())
        .unwrap_or_default())
}

fn hit(kind: &str, v: &Value) -> Option<SourceHit> {
    let id = id_of(v)?;
    let (title, creator) = match kind {
        "artist" => (v["name"].as_str()?.to_string(), None),
        _ => (
            v["title"].as_str()?.to_string(),
            v["artist"]["name"].as_str().map(str::to_string),
        ),
    };
    Some(SourceHit {
        id: format!("{kind}:{id}"),
        title,
        creator,
        year: year_of(v),
        downloads: match kind {
            "artist" => v["nb_fan"].as_u64(),
            "track" => v["rank"].as_u64(),
            _ => None,
        },
        image: cover(v),
        kind: kind.into(),
        // What plays: a 30 s preview, not the song's full length (fresh-eyes review)
        duration_ms: v["duration"].as_u64().map(|d| d.min(PREVIEW_SECS) * 1000),
        library_artist_id: None,
        isrc: v["isrc"].as_str().map(str::to_string),
        upc: v["upc"].as_str().map(str::to_string),
        library_track_id: None,
        library_album_id: None,
        uri: (kind == "track").then(|| track_uri(&id)),
        url: page_url(kind, &id),
    })
}

fn track(v: &Value, n: u32, artist: Option<&str>) -> Option<SourceTrack> {
    let id = id_of(v)?;
    Some(SourceTrack {
        title: v["title"].as_str()?.to_string(),
        track_no: v["track_position"].as_u64().map(|p| p as u32).or(Some(n)),
        duration_ms: v["duration"].as_u64().map(|d| d.min(PREVIEW_SECS) * 1000),
        format: "MP3 preview (30 s)".into(),
        url: track_uri(&id),
        artist: v["artist"]["name"].as_str().or(artist).map(str::to_string),
    })
}

fn album_item(v: &Value) -> Option<SourceItem> {
    let id = id_of(v)?;
    let artist = v["artist"]["name"].as_str().map(str::to_string);
    let tracks = v["tracks"]["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(i, t)| track(t, i as u32 + 1, artist.as_deref()))
                .collect()
        })
        .unwrap_or_default();
    Some(SourceItem {
        source: SOURCE_ID.into(),
        id: format!("album:{id}"),
        title: v["title"].as_str()?.to_string(),
        creator: artist,
        year: year_of(v),
        description: v["label"].as_str().map(|l| format!("Label: {l}")),
        licence: None,
        image: cover(v),
        url: page_url("album", &id),
        tracks,
        library_album_id: None,
    })
}

fn track_item(v: &Value) -> Option<SourceItem> {
    let id = id_of(v)?;
    let artist = v["artist"]["name"].as_str().map(str::to_string);
    Some(SourceItem {
        source: SOURCE_ID.into(),
        id: format!("track:{id}"),
        title: v["title"].as_str()?.to_string(),
        creator: artist.clone(),
        year: year_of(v).or_else(|| year_of(&v["album"])),
        description: v["album"]["title"].as_str().map(|a| format!("From {a}")),
        licence: None,
        image: cover(v),
        url: page_url("track", &id),
        tracks: track(v, 1, artist.as_deref()).into_iter().collect(),
        library_album_id: None,
    })
}

pub struct Deezer;

#[async_trait::async_trait]
impl StreamingService for Deezer {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn unified(&self, _s: &NodeState) -> bool {
        true
    }

    fn info(&self, _s: &NodeState) -> SourceInfo {
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Deezer".into(),
            description: "Catalogue, artists and 30-second previews without an account; full streams need a Deezer partner agreement".into(),
            status: "previews".into(),
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let q = |kind: &'static str, n: u32| async move {
            let v = get_json(
                s,
                &format!("/search/{kind}"),
                &[("q", term.to_string()), ("limit", n.to_string())],
            )
            .await?;
            Ok::<Vec<SourceHit>, ApiError>(
                v["data"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| hit(kind, x)).collect())
                    .unwrap_or_default(),
            )
        };
        let (artists, albums, tracks) =
            tokio::join!(q("artist", 5), q("album", limit.min(20)), q("track", limit));
        // Deezer ranks tribute acts and "DJ <name>" high; the exact name goes first,
        // and only a few artists are worth a chip.
        let needle = term.trim().to_lowercase();
        let mut artists = artists?;
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
        artists.truncate(3);
        let mut out = Vec::new();
        out.extend(artists);
        out.extend(albums?);
        out.extend(tracks?);
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        if let Some(a) = id.strip_prefix("album:") {
            let v = check(
                get_json(s, &format!("/album/{}", numeric(a)?), &[]).await?,
                "album",
            )?;
            return album_item(&v).ok_or_else(|| upstream("album without title"));
        }
        let t = id.strip_prefix("track:").unwrap_or(id);
        let v = check(
            get_json(s, &format!("/track/{}", numeric(t)?), &[]).await?,
            "track",
        )?;
        track_item(&v).ok_or_else(|| upstream("track without title"))
    }

    async fn artist(&self, s: &NodeState, id: &str) -> Result<SourceArtist, ApiError> {
        let a = numeric(id.strip_prefix("artist:").unwrap_or(id))?;
        let (p1, p2, q2) = (
            format!("/artist/{a}"),
            format!("/artist/{a}/albums"),
            [("limit", "100".to_string())],
        );
        let (ar, albums) = tokio::join!(get_json(s, &p1, &[]), get_json(s, &p2, &q2));
        let ar = check(ar?, "artist")?;
        let name = ar["name"].as_str().unwrap_or("").to_string();
        let mut albums: Vec<SourceHit> = albums?["data"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|x| {
                        let mut h = hit("album", x)?;
                        h.creator = Some(name.clone());
                        Some(h)
                    })
                    .collect()
            })
            .unwrap_or_default();
        albums.sort_by_key(|x| std::cmp::Reverse(x.year));
        Ok(SourceArtist {
            source: SOURCE_ID.into(),
            id: format!("artist:{a}"),
            name,
            image: cover(&ar),
            url: page_url("artist", a),
            albums,
            library_artist_id: None,
        })
    }
}

// ---- versions: the same release on Deezer, by UPC / ISRC ----

fn version(kind: &str, v: &Value) -> Option<Version> {
    let id = id_of(v)?;
    let mut label = format!("Deezer · {}", v["title"].as_str()?);
    if v["explicit_lyrics"].as_bool() == Some(true) {
        label.push_str(" (explicit)");
    }
    Some(Version {
        page: None,
        source: SOURCE_ID.into(),
        id: format!("{kind}:{id}"),
        label,
        url: page_url(kind, &id),
        uri: (kind == "track").then(|| track_uri(&id)),
    })
}

pub(crate) async fn album_versions(s: &NodeState, album: &pm_model::Album) -> Result<Vec<Version>, ApiError> {
    let Some(upc) = album.upc.as_ref() else {
        return Ok(Vec::new());
    };
    let key = format!("deezer.version.album.{}", album.id.unwrap_or(0));
    if let Some(v) = crate::tidal::cache_get(s, &key) {
        return Ok(v);
    }
    let out = match get_json(s, &format!("/album/upc:{upc}"), &[])
        .await
        .and_then(|v| check(v, "album"))
    {
        Ok(v) => version("album", &v).into_iter().collect(),
        Err(e) if e.status == StatusCode::NOT_FOUND => Vec::new(),
        Err(e) => return Err(e),
    };
    crate::tidal::cache_put(s, &key, &out);
    Ok(out)
}

pub(crate) async fn track_versions(s: &NodeState, track: &pm_model::Track) -> Result<Vec<Version>, ApiError> {
    let Some(isrc) = track.isrc.as_ref() else {
        return Ok(Vec::new());
    };
    isrc_versions(s, &isrc.to_string()).await
}

/// Deezer's track with this ISRC (public catalogue, no account; cached per ISRC).
/// The album of a Deezer track: (title, link, cover, year).
pub(crate) async fn track_album(
    s: &NodeState,
    id: &str,
) -> Option<(String, String, Option<String>, Option<i32>)> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = get_json(s, &format!("/track/{id}"), &[]).await.ok()?;
    let a = &v["album"];
    let aid = id_of(a)?;
    Some((
        a["title"].as_str()?.to_string(),
        page_url("album", &aid),
        cover(a),
        year_of(a).or_else(|| year_of(&v)),
    ))
}

/// A Deezer track's ISRC.
pub(crate) async fn track_isrc(s: &NodeState, id: &str) -> Option<String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = get_json(s, &format!("/track/{id}"), &[]).await.ok()?;
    v["isrc"].as_str().map(str::to_string)
}

pub(crate) async fn isrc_versions(s: &NodeState, isrc: &str) -> Result<Vec<Version>, ApiError> {
    let key = format!("deezer.version.isrc.{isrc}");
    if let Some(v) = crate::tidal::cache_get(s, &key) {
        return Ok(v);
    }
    let out = match get_json(s, &format!("/track/isrc:{isrc}"), &[])
        .await
        .and_then(|v| check(v, "track"))
    {
        Ok(v) => version("track", &v).into_iter().collect(),
        Err(e) if e.status == StatusCode::NOT_FOUND => Vec::new(),
        Err(e) => return Err(e),
    };
    crate::tidal::cache_put(s, &key, &out);
    Ok(out)
}

// ---- playback ----

/// The preview URL of a track (blocking; used on the audio thread).
pub(crate) fn preview_url_blocking(base: &SharedBase, id: &str) -> Result<String, String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("bad Deezer track id".into());
    }
    let b = base
        .read()
        .map_err(|_| "deezer base")?
        .trim_end_matches('/')
        .to_string();
    let v: Value = crate::sources::http_client()
        .get(format!("{b}/track/{id}"))
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    if let Some(e) = v.get("error") {
        return Err(format!(
            "Deezer: {}",
            e["message"].as_str().unwrap_or("no such track")
        ));
    }
    v["preview"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Deezer has no preview for this track".into())
}

pub(crate) fn open_track_blocking(
    base: &SharedBase,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let url = preview_url_blocking(base, id).map_err(|reason| pm_zones::SourceError::Open {
        uri: track_uri(id),
        reason,
    })?;
    crate::sources::open_http_stream(&url)
}

/// `GET /v1/sources/deezer/stream/{id}`: sends browsers to the preview.
pub(crate) async fn stream(
    axum::extract::State(s): axum::extract::State<Arc<NodeState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::response::Redirect, ApiError> {
    numeric(&id)?;
    let v = check(get_json(&s, &format!("/track/{id}"), &[]).await?, "track")?;
    let url = v["preview"]
        .as_str()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ApiError::not_found("Deezer has no preview for this track"))?;
    Ok(axum::response::Redirect::temporary(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_carry_identity_and_queue_uris() {
        let t: Value = serde_json::json!({"id": 138547415, "title": "Creep", "isrc": "GBAYE9200070", "duration": 238,
            "artist": {"name": "Radiohead"}, "album": {"cover_medium": "http://img/x.jpg", "release_date": "1993-02-22"}});
        let h = hit("track", &t).unwrap();
        assert_eq!(h.id, "track:138547415");
        assert_eq!(h.uri.as_deref(), Some("deezer:track:138547415"));
        assert_eq!(h.isrc.as_deref(), Some("GBAYE9200070"));
        assert_eq!(h.duration_ms, Some(30_000), "what plays: the preview");
        assert_eq!(h.image.as_deref(), Some("http://img/x.jpg"));
        let a: Value = serde_json::json!({"id": 1, "name": "Radiohead", "nb_fan": 7, "picture_big": "p"});
        let h = hit("artist", &a).unwrap();
        assert_eq!(
            (h.id.as_str(), h.title.as_str(), h.downloads),
            ("artist:1", "Radiohead", Some(7))
        );
        assert!(h.uri.is_none());
        assert!(check(
            serde_json::json!({"error": {"code": 800, "message": "no data"}}),
            "album"
        )
        .is_err());
    }
}
