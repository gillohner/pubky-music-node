//! Internet radio: the first streaming source that needs no account. A
//! station is a stream URL; the node plays it like any queue item (the
//! source factory decodes HTTP streams). Discovery goes through the
//! community directory radio-browser.info (HLS-only entries are left out:
//! the node plays continuous Icecast/SHOUTcast streams); saved stations live in the
//! library's settings (node-local for now; a `pubky.music/station` post is
//! the obvious later home).

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_zones::{Insert, QueueItem, ZoneId};
use std::sync::Arc;

const DIRECTORY: &str = "https://all.api.radio-browser.info/json/stations/search";
const SETTING: &str = "radio.stations";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Default)]
pub struct Station {
    /// Directory id when it came from radio-browser, else a hash of the URL.
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Station {
    /// The queue item that plays this station.
    pub fn queue_item(&self) -> QueueItem {
        let mut item = QueueItem::new(self.url.clone());
        item.title = Some(self.name.clone());
        let mut by = Vec::new();
        if let Some(c) = &self.country {
            by.push(c.clone());
        }
        if !self.tags.is_empty() {
            by.push(self.tags.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
        }
        item.artist = Some(by.join(" · ")).filter(|s| !s.is_empty());
        item.album = Some("Internet radio".into());
        item
    }

    fn with_id(mut self) -> Self {
        if self.id.trim().is_empty() {
            self.id = format!("url-{}", blake3_short(&self.url));
        }
        self
    }
}

fn blake3_short(s: &str) -> String {
    let h = blake3::hash(s.as_bytes());
    h.to_hex()[..16].to_string()
}

/// One radio-browser result, only the fields we use.
#[derive(Debug, serde::Deserialize)]
struct DirectoryStation {
    stationuuid: String,
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    url_resolved: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    favicon: String,
    #[serde(default)]
    codec: String,
    #[serde(default)]
    bitrate: u32,
    #[serde(default)]
    country: String,
    #[serde(default)]
    tags: String,
    /// 1 when the stream is HLS (segmented); the node plays continuous streams only.
    #[serde(default)]
    hls: u8,
}

impl From<DirectoryStation> for Station {
    fn from(d: DirectoryStation) -> Self {
        let url = if d.url_resolved.is_empty() {
            d.url
        } else {
            d.url_resolved
        };
        let non_empty = |s: String| if s.trim().is_empty() { None } else { Some(s) };
        Station {
            id: d.stationuuid,
            name: d.name.trim().to_string(),
            url,
            homepage: non_empty(d.homepage),
            favicon: non_empty(d.favicon),
            codec: non_empty(d.codec),
            bitrate: (d.bitrate > 0).then_some(d.bitrate),
            country: non_empty(d.country),
            tags: d
                .tags
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
        }
    }
}

/// Parse a radio-browser search response into stations with a playable URL.
pub(crate) fn parse_directory(json: &str) -> Result<Vec<Station>, serde_json::Error> {
    let rows: Vec<DirectoryStation> = serde_json::from_str(json)?;
    Ok(rows
        .into_iter()
        .filter(|d| d.hls == 0 && !d.url_resolved.to_ascii_lowercase().contains(".m3u8"))
        .map(Station::from)
        .filter(|s| s.url.starts_with("http://") || s.url.starts_with("https://"))
        .collect())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct RadioSearch {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stations {
    pub stations: Vec<Station>,
}

fn directory_client(s: &NodeState) -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let contact = s.contact();
    CLIENT.get_or_init(move || {
        let ua = if contact.is_empty() || contact == "unknown" {
            format!("pubky-music-node/{}", env!("CARGO_PKG_VERSION"))
        } else {
            format!("pubky-music-node/{} ({contact})", env!("CARGO_PKG_VERSION"))
        };
        reqwest::Client::builder()
            .user_agent(ua)
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("http client")
    })
}

/// `GET /v1/radio/search?q=&tag=&country=&limit=`: the directory, most voted first.
pub(crate) async fn search(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<RadioSearch>,
) -> ApiResult<Stations> {
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    let mut params: Vec<(&str, String)> = vec![
        ("limit", limit.to_string()),
        ("hidebroken", "true".into()),
        ("order", "votes".into()),
        ("reverse", "true".into()),
    ];
    if let Some(v) = q.q.filter(|v| !v.trim().is_empty()) {
        params.push(("name", v.trim().to_string()));
    }
    if let Some(v) = q.tag.filter(|v| !v.trim().is_empty()) {
        params.push(("tag", v.trim().to_string()));
    }
    if let Some(v) = q.country.filter(|v| !v.trim().is_empty()) {
        params.push(("country", v.trim().to_string()));
    }
    if params.len() == 4 {
        return Err(ApiError::bad_request("give q, tag or country"));
    }
    let resp = directory_client(&s)
        .get(DIRECTORY)
        .query(&params)
        .send()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("radio directory: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("radio directory answered {}", resp.status()),
        ));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("radio directory: {e}")))?;
    let stations = parse_directory(&text)
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("radio directory: {e}")))?;
    Ok(Json(Stations { stations }))
}

fn load_saved(s: &NodeState) -> Result<Vec<Station>, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(lib
        .get_setting(SETTING)?
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default())
}

fn store_saved(s: &NodeState, stations: &[Station]) -> Result<(), ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    lib.set_setting(
        SETTING,
        &serde_json::to_string(stations).expect("stations serialize"),
    )?;
    Ok(())
}

pub(crate) async fn saved(State(s): State<Arc<NodeState>>) -> ApiResult<Stations> {
    Ok(Json(Stations {
        stations: load_saved(&s)?,
    }))
}

/// `PUT /v1/radio/stations {station}`: save (or update) a station.
pub(crate) async fn save(State(s): State<Arc<NodeState>>, Json(st): Json<Station>) -> ApiResult<Stations> {
    if st.name.trim().is_empty() || !(st.url.starts_with("http://") || st.url.starts_with("https://")) {
        return Err(ApiError::bad_request("a station needs a name and an http(s) url"));
    }
    let st = st.with_id();
    let mut stations = load_saved(&s)?;
    stations.retain(|x| x.id != st.id && x.url != st.url);
    stations.insert(0, st);
    store_saved(&s, &stations)?;
    Ok(Json(Stations { stations }))
}

pub(crate) async fn remove(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<Stations> {
    let mut stations = load_saved(&s)?;
    let before = stations.len();
    stations.retain(|x| x.id != id);
    if stations.len() == before {
        return Err(ApiError::not_found(format!("station {id}")));
    }
    store_saved(&s, &stations)?;
    Ok(Json(Stations { stations }))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlayStation {
    pub zone: String,
    pub station: Station,
}

/// `POST /v1/radio/play {zone, station}`: replace the zone's queue with the
/// station and start it.
pub(crate) async fn play(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<PlayStation>,
) -> ApiResult<Enqueued> {
    let zone = s
        .zones
        .get(&ZoneId::new(&req.zone))
        .ok_or_else(|| ApiError::not_found(format!("no zone {}", req.zone)))?;
    zone.stop();
    zone.clear();
    let ids = zone.enqueue(vec![req.station.queue_item()], Insert::End);
    zone.play_index(0);
    Ok(Json(Enqueued { ids }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_rows_become_stations() {
        let json = r#"[{"stationuuid":"u1","name":" FIP ","url":"http://x/pl.m3u","url_resolved":"https://icecast.radiofrance.fr/fip-hifi.aac","homepage":"https://www.fip.fr","favicon":"","codec":"AAC","bitrate":192,"country":"France","tags":"eclectic,jazz,public radio","votes":9},
                      {"stationuuid":"u2","name":"Broken","url":"rtsp://no","url_resolved":"","codec":"","bitrate":0,"country":"","tags":""},
                      {"stationuuid":"u3","name":"HLS only","url":"https://hls.example/x.m3u8","url_resolved":"https://hls.example/x.m3u8","hls":1,"codec":"AAC","bitrate":320,"country":"","tags":""}]"#;
        let st = parse_directory(json).unwrap();
        assert_eq!(st.len(), 1, "non-http and HLS streams are dropped");
        let fip = &st[0];
        assert_eq!(fip.name, "FIP");
        assert_eq!(fip.url, "https://icecast.radiofrance.fr/fip-hifi.aac");
        assert_eq!(fip.favicon, None);
        assert_eq!(fip.bitrate, Some(192));
        assert_eq!(fip.tags, vec!["eclectic", "jazz", "public radio"]);
        let item = fip.queue_item();
        assert_eq!(item.uri, fip.url);
        assert_eq!(item.title.as_deref(), Some("FIP"));
        assert_eq!(
            item.artist.as_deref(),
            Some("France · eclectic, jazz, public radio")
        );
        let own = Station {
            name: "Mine".into(),
            url: "http://me/stream".into(),
            ..Default::default()
        }
        .with_id();
        assert!(own.id.starts_with("url-"));
    }
}
