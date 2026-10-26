//! YouTube, official mode (owner, 2026-10-22; STEWARD-DUAL-MODE.md): YouTube and YouTube Music
//! links play in the "This browser" zone through YouTube's own embedded player (IFrame Player
//! API), driven by the page's controls. The audio never leaves YouTube's player, so these items
//! cannot reach the node's zones; a `youtube:video:<id>` queued on a zone fails with that reason.
//! Ads and YouTube's terms apply. The unofficial mode (yt-dlp, for zones) comes later.
//!
//! Search is optional: with the owner's own YouTube Data API key (free, in Google Cloud), the
//! YouTube tab and the scoped search find music videos. It is never part of the unified search,
//! because each search spends 100 of the key's 10,000 daily quota units.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const SOURCE_ID: &str = "youtube";
pub const VIDEO_URI_PREFIX: &str = "youtube:video:";
/// Why a zone refuses a YouTube item.
pub const ZONE_REFUSAL: &str = "YouTube plays only in This browser (YouTube's own player)";
/// The owner's YouTube Data API key (a backup carries it only under the passphrase).
pub const KEY_SETTING: &str = "youtube.api_key";
const API_BASE: &str = "https://www.googleapis.com/youtube/v3";

fn base_override() -> &'static std::sync::RwLock<Option<String>> {
    static B: std::sync::OnceLock<std::sync::RwLock<Option<String>>> = std::sync::OnceLock::new();
    B.get_or_init(|| std::sync::RwLock::new(None))
}

/// Point search at another API (tests: a mock).
pub fn set_api_base(url: &str) {
    *base_override().write().expect("youtube base") = Some(url.trim_end_matches('/').to_string());
}

fn api_base() -> String {
    base_override()
        .read()
        .expect("youtube base")
        .clone()
        .unwrap_or_else(|| API_BASE.into())
}

/// Whether a YouTube Data key is set (the node's list of apps; the key itself is never shown).
pub(crate) fn has_key(s: &NodeState) -> bool {
    api_key(s).is_some()
}

fn api_key(s: &NodeState) -> Option<String> {
    let lib = s.library.lock().expect("library poisoned");
    lib.get_setting(KEY_SETTING)
        .ok()
        .flatten()
        .filter(|k| !k.trim().is_empty())
}

pub fn video_uri(id: &str) -> String {
    format!("{VIDEO_URI_PREFIX}{id}")
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

fn valid_video(id: &str) -> Result<&str, ApiError> {
    if id.len() != 11
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::bad_request("bad YouTube video id"));
    }
    Ok(id)
}

/// The few entities YouTube puts in snippet titles.
fn unescape(t: &str) -> String {
    t.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// `PT1H2M3S` to milliseconds.
fn iso_duration_ms(d: &str) -> Option<u64> {
    let t = d.strip_prefix("PT")?;
    let (mut total, mut n) = (0u64, String::new());
    for c in t.chars() {
        if c.is_ascii_digit() {
            n.push(c);
            continue;
        }
        let v: u64 = n.parse().ok()?;
        n.clear();
        total += v * match c {
            'H' => 3600,
            'M' => 60,
            'S' => 1,
            _ => return None,
        };
    }
    Some(total * 1000)
}

/// "Artist - Topic" channels are YouTube Music's auto-generated artist channels.
fn channel_artist(v: &Value) -> Option<String> {
    v["snippet"]["channelTitle"]
        .as_str()
        .map(|c| c.trim_end_matches(" - Topic").to_string())
}

fn thumb(v: &Value) -> Option<String> {
    let t = &v["snippet"]["thumbnails"];
    t["high"]["url"]
        .as_str()
        .or(t["medium"]["url"].as_str())
        .or(t["default"]["url"].as_str())
        .map(str::to_string)
}

async fn get(s: &NodeState, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
    let key = api_key(s).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "YouTube search needs your YouTube Data API key (Sources › YouTube)",
        )
    })?;
    let resp = s
        .http_client()
        .get(format!("{}/{path}", api_base()))
        .query(query)
        .query(&[("key", key.as_str())])
        .send()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("YouTube: {}", e.without_url())))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(v);
    }
    let reason = v["error"]["errors"][0]["reason"].as_str().unwrap_or_default();
    let msg = match reason {
        "quotaExceeded" | "dailyLimitExceeded" => {
            "YouTube's daily search quota for your key is used up; it resets at midnight Pacific time"
                .to_string()
        }
        "keyInvalid" | "keyExpired" => {
            "YouTube refused the API key: check it under Sources › YouTube".to_string()
        }
        "accessNotConfigured" => {
            "Turn on the YouTube Data API v3 for your key's Google Cloud project".to_string()
        }
        _ => format!(
            "YouTube answered {status}: {}",
            v["error"]["message"].as_str().unwrap_or("")
        ),
    };
    Err(ApiError::new(StatusCode::BAD_GATEWAY, msg))
}

/// Durations for search hits (one cheap `videos` call for all of them).
async fn durations(s: &NodeState, ids: &[String]) -> std::collections::HashMap<String, u64> {
    if ids.is_empty() {
        return Default::default();
    }
    let Ok(v) = get(
        s,
        "videos",
        &[("part", "contentDetails".into()), ("id", ids.join(","))],
    )
    .await
    else {
        return Default::default();
    };
    v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            Some((
                i["id"].as_str()?.to_string(),
                iso_duration_ms(i["contentDetails"]["duration"].as_str()?)?,
            ))
        })
        .collect()
}

pub struct YouTube;

#[async_trait::async_trait]
impl StreamingService for YouTube {
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
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "YouTube".into(),
            description: "YouTube and YouTube Music play in This browser, in YouTube's own player; its ads and terms apply. Search with your own YouTube Data API key".into(),
            status: format!(
                "browser{}{}",
                if api_key(s).is_some() { " + search" } else { "" },
                if crate::ytdlp::zones_on(s) { " + zones" } else { "" }
            ),
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let v = get(
            s,
            "search",
            &[
                ("part", "snippet".into()),
                ("type", "video".into()),
                ("videoCategoryId", "10".into()),
                ("videoEmbeddable", "true".into()),
                ("maxResults", limit.clamp(1, 25).to_string()),
                ("q", term.to_string()),
            ],
        )
        .await?;
        let items: Vec<&Value> = v["items"].as_array().into_iter().flatten().collect();
        let ids: Vec<String> = items
            .iter()
            .filter_map(|i| i["id"]["videoId"].as_str().map(str::to_string))
            .collect();
        let dur = durations(s, &ids).await;
        Ok(items
            .iter()
            .filter_map(|i| {
                let id = i["id"]["videoId"].as_str()?;
                Some(SourceHit {
                    id: format!("video:{id}"),
                    title: unescape(i["snippet"]["title"].as_str()?),
                    creator: channel_artist(i),
                    year: i["snippet"]["publishedAt"]
                        .as_str()
                        .and_then(|d| d.get(..4)?.parse().ok()),
                    downloads: None,
                    image: thumb(i),
                    kind: "track".into(),
                    duration_ms: dur.get(id).copied(),
                    library_artist_id: None,
                    isrc: None,
                    upc: None,
                    library_track_id: None,
                    library_album_id: None,
                    uri: Some(video_uri(id)),
                    url: watch_url(id),
                })
            })
            .collect())
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let vid = valid_video(id.strip_prefix("video:").unwrap_or(id))?;
        let v = get(
            s,
            "videos",
            &[("part", "snippet,contentDetails".into()), ("id", vid.into())],
        )
        .await?;
        let i = v["items"]
            .get(0)
            .ok_or_else(|| ApiError::not_found("YouTube has no such video"))?;
        let title = unescape(i["snippet"]["title"].as_str().unwrap_or_default());
        Ok(SourceItem {
            source: SOURCE_ID.into(),
            id: format!("video:{vid}"),
            title: title.clone(),
            creator: channel_artist(i),
            year: i["snippet"]["publishedAt"]
                .as_str()
                .and_then(|d| d.get(..4)?.parse().ok()),
            description: None,
            licence: None,
            image: thumb(i),
            url: watch_url(vid),
            tracks: vec![SourceTrack {
                title,
                track_no: None,
                duration_ms: i["contentDetails"]["duration"].as_str().and_then(iso_duration_ms),
                format: "YouTube (in This browser)".into(),
                url: video_uri(vid),
                artist: channel_artist(i),
            }],
            library_album_id: None,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct KeyStatus {
    /// A key is set (it never comes back).
    pub set: bool,
}

#[derive(Debug, Deserialize)]
pub struct KeyUpdate {
    /// Empty forgets it.
    pub key: String,
}

/// `GET /v1/sources/youtube/key`.
pub(crate) async fn get_key(State(s): State<Arc<NodeState>>) -> ApiResult<KeyStatus> {
    Ok(Json(KeyStatus {
        set: api_key(&s).is_some(),
    }))
}

/// `PUT /v1/sources/youtube/key {key}`: the owner's YouTube Data API key, checked with one
/// cheap call before it is kept.
pub(crate) async fn put_key(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<KeyUpdate>,
) -> ApiResult<KeyStatus> {
    let key = u.key.trim();
    if key.is_empty() {
        s.library
            .lock()
            .expect("library poisoned")
            .delete_setting(KEY_SETTING)?;
        crate::search::forget_answers();
        return Ok(Json(KeyStatus { set: false }));
    }
    if key.len() < 30
        || key.len() > 60
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::bad_request(
            "A YouTube Data API key is about 39 letters, digits, - and _",
        ));
    }
    // One unit of quota: does YouTube take the key?
    let resp = s
        .http_client()
        .get(format!("{}/videos", api_base()))
        .query(&[("part", "id"), ("id", "dQw4w9WgXcQ"), ("key", key)])
        .send()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("YouTube: {}", e.without_url())))?;
    if !resp.status().is_success() {
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        let reason = v["error"]["errors"][0]["reason"].as_str().unwrap_or_default();
        return Err(ApiError::bad_request(match reason {
            "accessNotConfigured" => {
                "Turn on the YouTube Data API v3 for this key's Google Cloud project, then save again"
                    .to_string()
            }
            _ => format!(
                "YouTube refused the key: {}",
                v["error"]["message"].as_str().unwrap_or(reason)
            ),
        }));
    }
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(KEY_SETTING, key)?;
    crate::search::forget_answers();
    Ok(Json(KeyStatus { set: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_titles_read_like_youtube_writes_them() {
        assert_eq!(iso_duration_ms("PT4M13S"), Some(253_000));
        assert_eq!(iso_duration_ms("PT1H2S"), Some(3_602_000));
        assert_eq!(iso_duration_ms("P1D"), None);
        assert_eq!(unescape("Rock &amp; Roll &#39;76"), "Rock & Roll '76");
    }
}
