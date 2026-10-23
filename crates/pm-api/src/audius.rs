//! Audius: a decentralised music platform with a public read API. No account
//! or key is needed to search and stream; the API asks for an `app_name`.
//! Discovery nodes are found through `api.audius.co`; a track streams via
//! `/v1/tracks/{id}/stream`, which redirects to the content node.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::ApiError;
use axum::http::StatusCode;
use std::sync::Mutex;

pub const SOURCE_ID: &str = "audius";
const APP_NAME: &str = "pubky-music-node";

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("audius: {e}"))
}

/// A discovery node: the configured base, else one from `api.audius.co`
/// (remembered for the process).
async fn host(s: &NodeState) -> Result<String, ApiError> {
    static CACHED: Mutex<Option<String>> = Mutex::new(None);
    let configured = s
        .audius_base
        .read()
        .expect("audius base")
        .trim_end_matches('/')
        .to_string();
    if !configured.is_empty() {
        return Ok(configured);
    }
    if let Some(h) = CACHED.lock().expect("audius host").clone() {
        return Ok(h);
    }
    let v: serde_json::Value = s
        .http_client()
        .get("https://api.audius.co")
        .send()
        .await
        .map_err(upstream)?
        .json()
        .await
        .map_err(upstream)?;
    let h = v["data"][0]
        .as_str()
        .ok_or_else(|| upstream("no discovery node"))?
        .trim_end_matches('/')
        .to_string();
    *CACHED.lock().expect("audius host") = Some(h.clone());
    Ok(h)
}

fn year_of(v: &serde_json::Value) -> Option<i32> {
    v["release_date"]
        .as_str()
        .or(v["created_at"].as_str())
        .and_then(|d| {
            // "Fri Oct 10 2025 ..." or ISO.
            d.split([' ', '-', 'T'])
                .find_map(|p| (p.len() == 4).then(|| p.parse::<i32>().ok()).flatten())
        })
}

/// A track document → hit.
pub(crate) fn hit(host: &str, t: &serde_json::Value) -> Option<SourceHit> {
    let id = t["id"].as_str()?.to_string();
    let permalink = t["permalink"].as_str().unwrap_or("");
    Some(SourceHit {
        title: t["title"].as_str().unwrap_or(&id).to_string(),
        creator: t["user"]["name"].as_str().map(str::to_string),
        year: year_of(t),
        downloads: t["play_count"].as_u64(),
        image: t["artwork"]["480x480"]
            .as_str()
            .or(t["artwork"]["150x150"].as_str())
            .map(str::to_string),
        kind: "track".into(),
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: Some(format!("{host}/v1/tracks/{id}/stream?app_name={APP_NAME}")),
        url: if permalink.is_empty() {
            format!("{host}/v1/tracks/{id}")
        } else {
            format!("https://audius.co{permalink}")
        },
        id,
    })
}

/// A track document → single-track item.
pub(crate) fn item(host: &str, t: &serde_json::Value) -> Option<SourceItem> {
    let h = hit(host, t)?;
    let secs = t["duration"].as_f64();
    let track = SourceTrack {
        title: h.title.clone(),
        track_no: Some(1),
        duration_ms: secs.map(|d| (d * 1000.0) as u64),
        format: "MP3 (Audius stream)".into(),
        url: format!("{host}/v1/tracks/{}/stream?app_name={APP_NAME}", h.id),
        artist: h.creator.clone(),
    };
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
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string),
        image: h.image.clone(),
        url: h.url.clone(),
        tracks: vec![track],
        library_album_id: None,
    })
}

pub struct Audius;

#[async_trait::async_trait]
impl StreamingService for Audius {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play"]
    }

    fn info(&self, _s: &NodeState) -> SourceInfo {
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Audius".into(),
            description: "Independent artists on a decentralised platform; streams without an account".into(),
            status: "ready".into(),
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let host = host(s).await?;
        let resp = s
            .http_client()
            .get(format!("{host}/v1/tracks/search"))
            .query(&[
                ("query", term),
                ("app_name", APP_NAME),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .map_err(upstream)?;
        if !resp.status().is_success() {
            return Err(upstream(format!("search answered {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(upstream)?;
        Ok(v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| hit(&host, t))
                    .take(limit as usize)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        if id.trim().is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(ApiError::bad_request("bad track id"));
        }
        let host = host(s).await?;
        let resp = s
            .http_client()
            .get(format!("{host}/v1/tracks/{id}"))
            .query(&[("app_name", APP_NAME)])
            .send()
            .await
            .map_err(upstream)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(ApiError::not_found(format!("no Audius track {id}")));
        }
        if !resp.status().is_success() {
            return Err(upstream(format!("track answered {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(upstream)?;
        item(&host, &v["data"]).ok_or_else(|| ApiError::not_found(format!("no Audius track {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_documents_parse() {
        let t = serde_json::json!({"id": "W0NJ0", "title": "Ambient Piano Interlude", "user": {"name": "Techwipe", "handle": "techwipe"},
            "duration": 90, "genre": "Electronic", "permalink": "/techwipe/ambient-piano-interlude-353852",
            "artwork": {"150x150": "https://img/150", "480x480": "https://img/480"}, "release_date": "Fri Oct 10 2025 10:00:00 GMT+0000", "play_count": 42});
        let h = hit("https://api.audius.co", &t).unwrap();
        assert_eq!(h.kind, "track");
        assert_eq!(h.url, "https://audius.co/techwipe/ambient-piano-interlude-353852");
        assert_eq!(h.year, Some(2025));
        assert_eq!(h.image.as_deref(), Some("https://img/480"));
        let it = item("https://api.audius.co", &t).unwrap();
        assert_eq!(it.tracks.len(), 1);
        assert_eq!(
            it.tracks[0].url,
            "https://api.audius.co/v1/tracks/W0NJ0/stream?app_name=pubky-music-node"
        );
        assert_eq!(it.tracks[0].duration_ms, Some(90_000));
        assert!(hit("h", &serde_json::json!({"title": "no id"})).is_none());
    }
}
