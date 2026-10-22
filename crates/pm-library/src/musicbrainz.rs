//! MusicBrainz web service client (ws/2), used to resolve service URLs,
//! ISRCs and fuzzy matches to recording MBIDs.
//!
//! Rules of the service that this client enforces: one request per second,
//! a User-Agent that names the application and a contact, and backoff on
//! 503/429. See <https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting>.

use crate::error::{LibraryError, Result};
use pm_model::{Isrc, Mbid, MusicEntity, MusicUri};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const DEFAULT_BASE: &str = "https://musicbrainz.org/ws/2";

#[derive(Debug, Clone)]
pub struct MusicBrainzConfig {
    pub base_url: String,
    /// Contact address or URL for the User-Agent, required by MusicBrainz.
    pub contact: String,
    pub min_interval: Duration,
    pub timeout: Duration,
}

impl MusicBrainzConfig {
    pub fn new(contact: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE.to_string(),
            contact: contact.into(),
            min_interval: Duration::from_millis(1000),
            timeout: Duration::from_secs(20),
        }
    }

    pub fn user_agent(&self) -> String {
        format!(
            "pubky-music-node/{} ({})",
            env!("CARGO_PKG_VERSION"),
            self.contact
        )
    }
}

#[derive(Clone)]
pub struct MusicBrainzClient {
    http: reqwest::Client,
    config: MusicBrainzConfig,
    last_request: Arc<Mutex<Option<Instant>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordingInfo {
    pub mbid: Mbid,
    pub title: String,
    pub artists: Vec<(Option<Mbid>, String)>,
    pub isrcs: Vec<Isrc>,
    pub length_ms: Option<u64>,
    /// Releases this recording appears on: (release MBID, title).
    pub releases: Vec<(Mbid, String)>,
}

impl MusicBrainzClient {
    /// Refuses to start without a usable contact: MusicBrainz requires one
    /// and blocks agents that name a code host.
    pub fn new(config: MusicBrainzConfig) -> Result<Self> {
        let c = config.contact.trim().to_ascii_lowercase();
        if c.is_empty()
            || c == "unknown"
            || c.contains("github")
            || !(c.contains('@') || c.starts_with("http"))
        {
            return Err(LibraryError::MusicBrainz(
                "MusicBrainz needs a contact (email or URL, not a github link) in the User-Agent".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent(config.user_agent())
            .timeout(config.timeout)
            .build()
            .map_err(|e| LibraryError::MusicBrainz(e.to_string()))?;
        Ok(Self {
            http,
            config,
            last_request: Arc::new(Mutex::new(None)),
        })
    }

    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(t) = *last {
            let elapsed = t.elapsed();
            if elapsed < self.config.min_interval {
                tokio::time::sleep(self.config.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// GET a ws/2 path with query pairs. `Ok(None)` on 404.
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Option<Value>> {
        let mut url = url::Url::parse(&format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        ))
        .map_err(|e| LibraryError::MusicBrainz(e.to_string()))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
            pairs.append_pair("fmt", "json");
        }
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            self.throttle().await;
            let resp = self
                .http
                .get(url.clone())
                .send()
                .await
                .map_err(|e| LibraryError::MusicBrainz(e.to_string()))?;
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            {
                if attempt >= 3 {
                    return Err(LibraryError::MusicBrainz(format!(
                        "{url}: {status} after {attempt} attempts"
                    )));
                }
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_secs(2 * attempt as u64));
                tracing::warn!(%url, %status, ?wait, "musicbrainz asked us to back off");
                tokio::time::sleep(wait).await;
                continue;
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(LibraryError::MusicBrainz(format!(
                    "{url}: {status}: {}",
                    body.chars().take(200).collect::<String>()
                )));
            }
            let v: Value = resp
                .json()
                .await
                .map_err(|e| LibraryError::MusicBrainz(e.to_string()))?;
            return Ok(Some(v));
        }
    }

    pub async fn recording_by_mbid(&self, mbid: Mbid) -> Result<Option<RecordingInfo>> {
        let v = self
            .get(&format!("recording/{mbid}"), &[("inc", "artists+releases+isrcs")])
            .await?;
        v.map(|v| parse_recording(&v)).transpose()
    }

    /// Recordings carrying this ISRC (usually one, sometimes several).
    pub async fn recordings_by_isrc(&self, isrc: &Isrc) -> Result<Vec<Mbid>> {
        let v = self.get(&format!("isrc/{isrc}"), &[]).await?;
        Ok(v.map(|v| parse_isrc_recordings(&v)).unwrap_or_default())
    }

    /// Entities linked to a URL in MusicBrainz's URL relationships. Empty if
    /// MusicBrainz does not know the URL.
    pub async fn lookup_url(&self, service_url: &str) -> Result<Vec<(MusicEntity, Mbid)>> {
        let v = self
            .get(
                "url",
                &[
                    ("resource", service_url),
                    (
                        "inc",
                        "recording-rels+release-rels+artist-rels+release-group-rels",
                    ),
                ],
            )
            .await?;
        Ok(v.map(|v| parse_url_relations(&v)).unwrap_or_default())
    }

    /// Any ws/2 lookup or browse as JSON (paced and identified like every call);
    /// `Ok(None)` on 404.
    pub async fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Option<Value>> {
        self.get(path, query).await
    }

    /// Search recordings by artist and title; `duration_ms` narrows to ± 2 s.
    /// Returns (MBID, score 0–100) as MusicBrainz ranks them.
    pub async fn search_recording(
        &self,
        artist: &str,
        title: &str,
        duration_ms: Option<u64>,
    ) -> Result<Vec<(Mbid, u32)>> {
        let mut query = format!(
            "recording:\"{}\" AND artist:\"{}\"",
            lucene_escape(title),
            lucene_escape(artist)
        );
        if let Some(d) = duration_ms {
            query.push_str(&format!(" AND dur:[{} TO {}]", d.saturating_sub(2000), d + 2000));
        }
        let v = self
            .get("recording", &[("query", &query), ("limit", "10")])
            .await?;
        Ok(v.map(|v| parse_search(&v)).unwrap_or_default())
    }

    /// Like [`Self::search_recording`] but with what MusicBrainz says about each hit (title,
    /// artist, a release, length), for the owner to choose from. When nothing has a length within
    /// 2 s of `duration_ms` (a clipped, live or differently mastered file) it searches again
    /// without the length, so there is still something to choose from.
    pub async fn search_recording_hits(
        &self,
        artist: &str,
        title: &str,
        duration_ms: Option<u64>,
    ) -> Result<Vec<RecordingHit>> {
        let base = if artist.trim().is_empty() {
            format!("recording:\"{}\"", lucene_escape(title))
        } else {
            format!(
                "recording:\"{}\" AND artist:\"{}\"",
                lucene_escape(title),
                lucene_escape(artist)
            )
        };
        if let Some(d) = duration_ms {
            let q = format!("{base} AND dur:[{} TO {}]", d.saturating_sub(2000), d + 2000);
            let v = self.get("recording", &[("query", &q), ("limit", "10")]).await?;
            let hits = v.map(|v| parse_search_hits(&v)).unwrap_or_default();
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
        let v = self
            .get("recording", &[("query", &base), ("limit", "10")])
            .await?;
        Ok(v.map(|v| parse_search_hits(&v)).unwrap_or_default())
    }

    /// Resolve a service link or ISRC URI to a canonical recording URI.
    pub async fn canonicalize(&self, uri: &MusicUri) -> Result<Option<MusicUri>> {
        match uri {
            u if u.is_canonical() => Ok(Some(u.clone())),
            MusicUri::Isrc { isrc } => {
                let ids = self.recordings_by_isrc(isrc).await?;
                Ok(ids.first().map(|m| MusicUri::Recording { mbid: *m }))
            }
            MusicUri::Service(link) => {
                let rels = self.lookup_url(&link.url).await?;
                Ok(rels.into_iter().next().map(|(entity, mbid)| match entity {
                    MusicEntity::Recording => MusicUri::Recording { mbid },
                    MusicEntity::Release => MusicUri::Release { mbid },
                    MusicEntity::ReleaseGroup => MusicUri::ReleaseGroup { mbid },
                    MusicEntity::Artist => MusicUri::Artist { mbid },
                }))
            }
            _ => Ok(None),
        }
    }
}

/// Escape Lucene special characters inside a quoted phrase.
pub fn lucene_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn mbid_at(v: &Value, key: &str) -> Option<Mbid> {
    str_at(v, key).and_then(|s| Mbid::parse(s).ok())
}

pub fn parse_recording(v: &Value) -> Result<RecordingInfo> {
    let mbid = mbid_at(v, "id").ok_or_else(|| LibraryError::MusicBrainz("recording without id".into()))?;
    let title = str_at(v, "title").unwrap_or_default().to_string();
    let artists = v
        .get("artist-credit")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let artist = c.get("artist")?;
                    Some((
                        mbid_at(artist, "id"),
                        str_at(c, "name").or_else(|| str_at(artist, "name"))?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let isrcs = v
        .get("isrcs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter_map(|s| Isrc::parse(s).ok())
                .collect()
        })
        .unwrap_or_default();
    let length_ms = v.get("length").and_then(Value::as_u64);
    let releases = v
        .get("releases")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some((
                        mbid_at(r, "id")?,
                        str_at(r, "title").unwrap_or_default().to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(RecordingInfo {
        mbid,
        title,
        artists,
        isrcs,
        length_ms,
        releases,
    })
}

pub fn parse_isrc_recordings(v: &Value) -> Vec<Mbid> {
    v.get("recordings")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|r| mbid_at(r, "id")).collect())
        .unwrap_or_default()
}

pub fn parse_url_relations(v: &Value) -> Vec<(MusicEntity, Mbid)> {
    let mut out = Vec::new();
    for rel in v.get("relations").and_then(Value::as_array).into_iter().flatten() {
        let target = str_at(rel, "target-type").unwrap_or_default();
        let (entity, key) = match target {
            "recording" => (MusicEntity::Recording, "recording"),
            "release" => (MusicEntity::Release, "release"),
            "release_group" | "release-group" => (MusicEntity::ReleaseGroup, "release_group"),
            "artist" => (MusicEntity::Artist, "artist"),
            _ => continue,
        };
        let obj = rel.get(key).or_else(|| rel.get("release-group"));
        if let Some(m) = obj.and_then(|o| mbid_at(o, "id")) {
            out.push((entity, m));
        }
    }
    // Recording links first: they are the most specific.
    out.sort_by_key(|(e, _)| match e {
        MusicEntity::Recording => 0,
        MusicEntity::Release => 1,
        MusicEntity::ReleaseGroup => 2,
        MusicEntity::Artist => 3,
    });
    out
}

pub fn parse_search(v: &Value) -> Vec<(Mbid, u32)> {
    v.get("recordings")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some((
                        mbid_at(r, "id")?,
                        r.get("score").and_then(Value::as_u64).unwrap_or(0) as u32,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A recording search hit with what MusicBrainz says about it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecordingHit {
    pub mbid: Mbid,
    pub score: u32,
    pub title: String,
    pub artist: String,
    /// The first release it is on, when MusicBrainz lists one.
    pub release: Option<String>,
    pub release_mbid: Option<Mbid>,
    pub length_ms: Option<u64>,
}

pub fn parse_search_hits(v: &Value) -> Vec<RecordingHit> {
    v.get("recordings")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let artist = r
                        .get("artist-credit")
                        .and_then(Value::as_array)
                        .map(|c| {
                            c.iter()
                                .map(|x| {
                                    format!(
                                        "{}{}",
                                        str_at(x, "name").unwrap_or_default(),
                                        str_at(x, "joinphrase").unwrap_or_default()
                                    )
                                })
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    let release = r
                        .get("releases")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first());
                    Some(RecordingHit {
                        mbid: mbid_at(r, "id")?,
                        score: r.get("score").and_then(Value::as_u64).unwrap_or(0) as u32,
                        title: str_at(r, "title").unwrap_or_default().to_string(),
                        artist,
                        release: release.and_then(|x| str_at(x, "title")).map(str::to_string),
                        release_mbid: release.and_then(|x| mbid_at(x, "id")),
                        length_ms: r.get("length").and_then(Value::as_u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const MB2: &str = "0383dadf-2a4e-4d10-a46a-e9e041da8eb3";

    #[test]
    fn recording_mapping() {
        let v = json!({
            "id": MB, "title": "Get Lucky", "length": 369_000,
            "artist-credit": [{"name": "Daft Punk", "artist": {"id": MB2, "name": "Daft Punk"}}, {"name": "Pharrell Williams", "artist": {"id": "bad", "name": "Pharrell"}}],
            "isrcs": ["USQX91300108", "nope"],
            "releases": [{"id": MB2, "title": "Random Access Memories"}, {"id": "x", "title": "ignored"}]
        });
        let r = parse_recording(&v).unwrap();
        assert_eq!(r.mbid, Mbid::parse(MB).unwrap());
        assert_eq!(r.title, "Get Lucky");
        assert_eq!(r.length_ms, Some(369_000));
        assert_eq!(r.artists.len(), 2);
        assert_eq!(
            r.artists[0],
            (Some(Mbid::parse(MB2).unwrap()), "Daft Punk".into())
        );
        assert_eq!(r.artists[1], (None, "Pharrell Williams".into()));
        assert_eq!(r.isrcs, vec![Isrc::parse("USQX91300108").unwrap()]);
        assert_eq!(r.releases.len(), 1);
        assert!(parse_recording(&json!({"title": "no id"})).is_err());
    }

    #[test]
    fn isrc_url_and_search_mapping() {
        let v = json!({"isrc": "USQX91300108", "recordings": [{"id": MB}, {"id": "junk"}]});
        assert_eq!(parse_isrc_recordings(&v), vec![Mbid::parse(MB).unwrap()]);

        let v = json!({"resource": "https://open.spotify.com/track/x", "relations": [
            {"target-type": "artist", "artist": {"id": MB2}},
            {"target-type": "recording", "recording": {"id": MB}},
            {"target-type": "release_group", "release_group": {"id": MB2}}
        ]});
        let rels = parse_url_relations(&v);
        assert_eq!(rels[0], (MusicEntity::Recording, Mbid::parse(MB).unwrap()));
        assert_eq!(rels[1], (MusicEntity::ReleaseGroup, Mbid::parse(MB2).unwrap()));
        assert_eq!(rels[2], (MusicEntity::Artist, Mbid::parse(MB2).unwrap()));

        let v = json!({"recordings": [{"id": MB, "score": 100}, {"id": MB2, "score": 87}]});
        assert_eq!(
            parse_search(&v),
            vec![(Mbid::parse(MB).unwrap(), 100), (Mbid::parse(MB2).unwrap(), 87)]
        );
    }

    #[test]
    fn lucene_escaping_and_user_agent() {
        assert_eq!(
            lucene_escape("AC/DC (Live) \"x\""),
            "AC\\/DC \\(Live\\) \\\"x\\\""
        );
        let c = MusicBrainzConfig::new("dev@example.org");
        assert!(c.user_agent().starts_with("pubky-music-node/"));
        assert!(c.user_agent().ends_with("(dev@example.org)"));
    }

    /// Live check against musicbrainz.org; runs only with PM_LIVE_TESTS=1.
    #[tokio::test]
    async fn live_recording_lookup() {
        if std::env::var("PM_LIVE_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping live MusicBrainz test (set PM_LIVE_TESTS=1)");
            return;
        }
        let client = MusicBrainzClient::new(MusicBrainzConfig::new(
            "pubky-music-node tests, claude@riginode.xyz",
        ))
        .unwrap();
        // Daft Punk feat. Pharrell Williams – "Get Lucky" (album version).
        let mbid = Mbid::parse(LIVE_GET_LUCKY_MBID).unwrap();
        let r = client
            .recording_by_mbid(mbid)
            .await
            .unwrap()
            .expect("recording exists");
        assert_eq!(r.mbid, mbid);
        assert!(
            r.title.to_lowercase().contains("get lucky"),
            "title was {}",
            r.title
        );
        assert!(r.artists.iter().any(|(_, n)| n.contains("Daft Punk")));
        assert!(client
            .recording_by_mbid(Mbid::parse("ffffffff-ffff-4fff-8fff-ffffffffffff").unwrap())
            .await
            .unwrap()
            .is_none());
    }

    const LIVE_GET_LUCKY_MBID: &str = "833f00e1-781f-4edd-90e4-e52712618862";
}
