//! Internet radio as a source (`radio`): the radio-browser.info directory (search, countries,
//! tags), SomaFM's channels and Radio Paradise's channels, five of them lossless FLAC. All
//! three are free and official: radio-browser's public API, SomaFM's `channels.json`, Radio
//! Paradise's published stream mounts. No account.
//!
//! Station ids: radio-browser's station UUID, `somafm:<channel>`, `rp:<channel>`. A station is
//! one stream URL; zones play it like any other queue item (SomaFM's `.pls` playlists are
//! resolved by the HTTP source, and in-stream titles show as "now playing").
//!
//! The channel lists follow Music Assistant's SomaFM and Radio Paradise providers
//! (Apache-2.0, github.com/music-assistant/server): which mounts carry FLAC with titles.

use crate::radio::{parse_directory, Station};
use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SOURCE_ID: &str = "radio";
const DIRECTORY: &str = "https://all.api.radio-browser.info/json";
const SOMAFM: &str = "https://somafm.com/channels.json";
/// SomaFM's list changes rarely.
const SOMAFM_TTL: Duration = Duration::from_secs(3600);

/// Radio Paradise's channels: `-flacm` mounts are FLAC (in Ogg) with stream titles.
const RADIO_PARADISE: &[(&str, &str, &str, &str)] = &[
    (
        "0",
        "Radio Paradise · Main Mix",
        "Eclectic mix, hand-picked by real humans",
        "https://stream.radioparadise.com/flacm",
    ),
    (
        "1",
        "Radio Paradise · Mellow Mix",
        "A mellower selection from the RP library",
        "https://stream.radioparadise.com/mellow-flacm",
    ),
    (
        "2",
        "Radio Paradise · Rock Mix",
        "Heavier selections from the RP library",
        "https://stream.radioparadise.com/rock-flacm",
    ),
    (
        "3",
        "Radio Paradise · Global",
        "Global music and experimental selections",
        "https://stream.radioparadise.com/global-flacm",
    ),
    (
        "4",
        "Radio Paradise · Beyond",
        "Exploring the frontiers of improvisational music",
        "https://stream.radioparadise.com/beyond-flacm",
    ),
    (
        "5",
        "Radio Paradise · Serenity",
        "Ambient and nature sounds",
        "https://stream.radioparadise.com/serenity",
    ),
];

pub fn radio_paradise() -> Vec<Station> {
    RADIO_PARADISE
        .iter()
        .map(|(id, name, desc, url)| Station {
            id: format!("rp:{id}"),
            name: (*name).into(),
            url: (*url).into(),
            homepage: Some("https://radioparadise.com".into()),
            favicon: None,
            codec: Some(if url.ends_with("flacm") { "FLAC" } else { "AAC" }.into()),
            bitrate: None,
            country: Some("United States".into()),
            tags: vec![(*desc).to_string()],
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct SomaChannels {
    channels: Vec<SomaChannel>,
}

#[derive(serde::Deserialize)]
struct SomaChannel {
    id: String,
    title: String,
    #[serde(default)]
    genre: String,
    #[serde(default)]
    image: String,
    #[serde(default, rename = "largeimage")]
    large_image: String,
    #[serde(default)]
    playlists: Vec<SomaPlaylist>,
}

#[derive(serde::Deserialize)]
struct SomaPlaylist {
    url: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    quality: String,
}

/// SomaFM's `channels.json` as stations: the highest-quality MP3 playlist, else the best AAC.
pub fn parse_somafm(json: &str) -> Result<Vec<Station>, serde_json::Error> {
    let list: SomaChannels = serde_json::from_str(json)?;
    let rank = |p: &SomaPlaylist| {
        let q = match p.quality.as_str() {
            "highest" => 0,
            "high" => 1,
            _ => 2,
        };
        let f = match p.format.as_str() {
            "mp3" => 0,
            "aac" => 1,
            _ => 2,
        };
        (q, f)
    };
    Ok(list
        .channels
        .into_iter()
        .filter_map(|c| {
            let best = c.playlists.iter().min_by_key(|p| rank(p))?;
            Some(Station {
                id: format!("somafm:{}", c.id),
                name: format!("SomaFM · {}", c.title),
                url: best.url.clone(),
                homepage: Some(format!("https://somafm.com/{}/", c.id)),
                favicon: Some(if c.large_image.is_empty() {
                    c.image
                } else {
                    c.large_image
                })
                .filter(|s| !s.is_empty()),
                codec: Some(best.format.to_uppercase()).filter(|s| !s.is_empty()),
                bitrate: None,
                country: Some("United States".into()),
                tags: c
                    .genre
                    .split('|')
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty())
                    .collect(),
            })
        })
        .collect())
}

static SOMAFM_CACHE: Mutex<Option<(Instant, Vec<Station>)>> = Mutex::new(None);

async fn somafm(s: &NodeState) -> Result<Vec<Station>, ApiError> {
    if let Some((at, list)) = SOMAFM_CACHE.lock().expect("somafm cache").as_ref() {
        if at.elapsed() < SOMAFM_TTL {
            return Ok(list.clone());
        }
    }
    let text = get_text(s, SOMAFM, &[]).await?;
    let list = parse_somafm(&text).map_err(|e| bad_gateway(format!("SomaFM: {e}")))?;
    *SOMAFM_CACHE.lock().expect("somafm cache") = Some((Instant::now(), list.clone()));
    Ok(list)
}

fn bad_gateway(m: String) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, m)
}

async fn get_text(s: &NodeState, url: &str, params: &[(&str, String)]) -> Result<String, ApiError> {
    let resp = s
        .http_client()
        .get(url)
        .query(params)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| bad_gateway(format!("{url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(bad_gateway(format!("{url} answered {}", resp.status())));
    }
    resp.text().await.map_err(|e| bad_gateway(format!("{url}: {e}")))
}

/// The curated stations: SomaFM, then Radio Paradise.
pub async fn curated(s: &NodeState) -> Vec<Station> {
    let mut out = match somafm(s).await {
        Ok(v) => v,
        Err(e) => {
            tracing::info!("SomaFM channels unavailable: {}", e.message);
            Vec::new()
        }
    };
    out.extend(radio_paradise());
    out
}

/// `GET /v1/radio/curated`: SomaFM's channels and Radio Paradise's.
pub(crate) async fn curated_route(State(s): State<Arc<NodeState>>) -> ApiResult<crate::radio::Stations> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    Ok(Json(crate::radio::Stations {
        stations: curated(&s).await,
    }))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Facet {
    pub name: String,
    /// ISO 3166-1 alpha-2, for countries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub stations: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Facets {
    pub items: Vec<Facet>,
}

#[derive(serde::Deserialize)]
struct DirFacet {
    name: String,
    #[serde(default)]
    iso_3166_1: Option<String>,
    #[serde(default)]
    stationcount: u64,
}

pub fn parse_facets(json: &str) -> Result<Vec<Facet>, serde_json::Error> {
    let rows: Vec<DirFacet> = serde_json::from_str(json)?;
    Ok(rows
        .into_iter()
        .filter(|r| r.stationcount > 0 && !r.name.trim().is_empty())
        .map(|r| Facet {
            name: r.name.trim().to_string(),
            code: r.iso_3166_1.filter(|c| !c.is_empty()),
            stations: r.stationcount,
        })
        .collect())
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct FacetQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

async fn facets(s: &NodeState, kind: &str, limit: u32) -> Result<Vec<Facet>, ApiError> {
    let text = get_text(
        s,
        &format!("{DIRECTORY}/{kind}"),
        &[
            ("order", "stationcount".into()),
            ("reverse", "true".into()),
            ("hidebroken", "true".into()),
            ("limit", limit.to_string()),
        ],
    )
    .await?;
    parse_facets(&text).map_err(|e| bad_gateway(format!("radio directory: {e}")))
}

/// `GET /v1/radio/countries?limit=`: countries with stations, most first.
pub(crate) async fn countries(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<FacetQuery>,
) -> ApiResult<Facets> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    Ok(Json(Facets {
        items: facets(&s, "countries", q.limit.unwrap_or(250).clamp(1, 300)).await?,
    }))
}

/// `GET /v1/radio/tags?limit=`: the most used station tags (genres, moods, languages).
pub(crate) async fn tags(State(s): State<Arc<NodeState>>, Query(q): Query<FacetQuery>) -> ApiResult<Facets> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    Ok(Json(Facets {
        items: facets(&s, "tags", q.limit.unwrap_or(100).clamp(1, 500)).await?,
    }))
}

/// One station by id, wherever it comes from.
pub async fn station(s: &NodeState, id: &str) -> Result<Station, ApiError> {
    if id.starts_with("rp:") || id.starts_with("somafm:") {
        return curated(s)
            .await
            .into_iter()
            .find(|st| st.id == id)
            .ok_or_else(|| ApiError::not_found(format!("no station {id}")));
    }
    if let Some(st) = crate::radio::load_saved(s)?.into_iter().find(|st| st.id == id) {
        return Ok(st);
    }
    let text = get_text(s, &format!("{DIRECTORY}/stations/byuuid/{id}"), &[]).await?;
    parse_directory(&text)
        .map_err(|e| bad_gateway(format!("radio directory: {e}")))?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found(format!("no station {id}")))
}

fn hit(st: &Station) -> SourceHit {
    let item = st.queue_item();
    SourceHit {
        id: st.id.clone(),
        title: st.name.clone(),
        creator: item.artist,
        year: None,
        downloads: None,
        image: st.favicon.clone(),
        kind: "track".into(),
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: Some(st.url.clone()),
        url: st.homepage.clone().unwrap_or_else(|| st.url.clone()),
    }
}

pub struct Radio;

#[async_trait::async_trait]
impl StreamingService for Radio {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn info(&self, _s: &NodeState) -> SourceInfo {
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Internet radio".into(),
            description: "Tens of thousands of stations from radio-browser.info, SomaFM, and Radio Paradise in lossless FLAC; no account".into(),
            status: "ready".into(),
        }
    }

    /// Stations whose name matches: the curated ones first, then the directory's most voted.
    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let t = term.to_lowercase();
        let mut out: Vec<SourceHit> = curated(s)
            .await
            .iter()
            .filter(|st| st.name.to_lowercase().contains(&t) || st.tags.iter().any(|g| g.to_lowercase() == t))
            .map(hit)
            .collect();
        let text = get_text(
            s,
            &format!("{DIRECTORY}/stations/search"),
            &[
                ("name", term.to_string()),
                ("limit", limit.to_string()),
                ("hidebroken", "true".into()),
                ("order", "votes".into()),
                ("reverse", "true".into()),
            ],
        )
        .await;
        match text {
            Ok(text) => out.extend(
                parse_directory(&text)
                    .map_err(|e| bad_gateway(format!("radio directory: {e}")))?
                    .iter()
                    .map(hit),
            ),
            // The curated stations still answer when the directory is down.
            Err(e) if !out.is_empty() => tracing::info!("radio directory: {}", e.message),
            Err(e) => return Err(e),
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let st = station(s, id).await?;
        let q = st.queue_item();
        let format = match (&st.codec, st.bitrate) {
            (Some(c), Some(b)) => format!("{c} {b} kbps"),
            (Some(c), None) => c.clone(),
            (None, Some(b)) => format!("{b} kbps"),
            (None, None) => "stream".into(),
        };
        Ok(SourceItem {
            source: SOURCE_ID.into(),
            id: st.id.clone(),
            title: st.name.clone(),
            creator: q.artist.clone(),
            year: None,
            description: Some(st.tags.join(", ")).filter(|d| !d.is_empty()),
            licence: None,
            image: st.favicon.clone(),
            url: st.homepage.clone().unwrap_or_else(|| st.url.clone()),
            tracks: vec![SourceTrack {
                title: st.name.clone(),
                track_no: None,
                duration_ms: None,
                format,
                url: st.url.clone(),
                artist: q.artist,
            }],
            library_album_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn somafm_channels_pick_the_best_stream() {
        let json = r#"{"channels":[
          {"id":"groovesalad","title":"Groove Salad","genre":"ambient|electronica","image":"https://api.somafm.com/img/groovesalad120.png","largeimage":"https://api.somafm.com/logos/256/groovesalad256.png",
           "playlists":[{"url":"https://api.somafm.com/groovesalad130.pls","format":"aac","quality":"highest"},
                        {"url":"https://api.somafm.com/groovesalad256.pls","format":"mp3","quality":"highest"},
                        {"url":"https://api.somafm.com/groovesalad.pls","format":"mp3","quality":"high"}]},
          {"id":"empty","title":"No streams","playlists":[]}]}"#;
        let st = parse_somafm(json).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].id, "somafm:groovesalad");
        assert_eq!(st[0].name, "SomaFM · Groove Salad");
        assert_eq!(st[0].url, "https://api.somafm.com/groovesalad256.pls");
        assert_eq!(st[0].codec.as_deref(), Some("MP3"));
        assert_eq!(st[0].tags, vec!["ambient", "electronica"]);
        assert_eq!(
            st[0].favicon.as_deref(),
            Some("https://api.somafm.com/logos/256/groovesalad256.png")
        );
    }

    #[test]
    fn radio_paradise_and_facets() {
        let rp = radio_paradise();
        assert_eq!(rp.len(), 6);
        assert_eq!(
            rp.iter().filter(|s| s.codec.as_deref() == Some("FLAC")).count(),
            5
        );
        assert!(rp
            .iter()
            .all(|s| s.id.starts_with("rp:") && s.url.starts_with("https://")));
        let f = parse_facets(r#"[{"name":"Switzerland","iso_3166_1":"CH","stationcount":850},{"name":"","stationcount":3},{"name":"jazz","stationcount":0}]"#).unwrap();
        assert_eq!(
            f,
            vec![Facet {
                name: "Switzerland".into(),
                code: Some("CH".into()),
                stations: 850
            }]
        );
        let h = hit(&rp[0]);
        assert_eq!((h.id.as_str(), h.kind.as_str()), ("rp:0", "track"));
    }
}
