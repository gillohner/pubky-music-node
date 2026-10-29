//! MusicBrainz web service client (ws/2), used to resolve service URLs,
//! ISRCs and fuzzy matches to recording MBIDs.
//!
//! Rules of the service that this client enforces: one request per second,
//! a User-Agent that names the application and a contact, and backoff on
//! 503/429. See <https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting>.

use crate::error::{LibraryError, Result};
use pm_model::{Isrc, Mbid, MusicEntity, MusicUri};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// The longest a 503's Retry-After holds every caller back.
const MAX_BACK_OFF: Duration = Duration::from_secs(60);

/// When the next request to a MusicBrainz host may go out.
struct Gate {
    next: Option<Instant>,
}

/// One gate per host (and port) for the whole process: MusicBrainz counts requests per IP, and
/// the node asks it from many places (identify, discovery, artist pages, new releases, the
/// proxy), each with its own client.
static GATES: LazyLock<std::sync::Mutex<HashMap<String, Arc<Mutex<Gate>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// The right to send one request to a MusicBrainz host. Nobody else asks that host while it is
/// held, and the next turn starts no sooner than `min_interval` after this one started (later
/// after [`Turn::back_off`]).
pub struct Turn {
    gate: tokio::sync::OwnedMutexGuard<Gate>,
    min_interval: Duration,
    started: Instant,
}

impl Turn {
    /// MusicBrainz asked to slow down (503/429): nobody asks this host again before `wait`.
    pub fn back_off(&mut self, wait: Duration) {
        let wait = wait
            .max(self.min_interval)
            .min(MAX_BACK_OFF.max(self.min_interval));
        self.gate.next = Some(Instant::now() + wait);
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        // Spaced from the start of this request, not its answer: a slow answer must not halve
        // the rate (requests still go one at a time).
        let after = self.started + self.min_interval;
        if self.gate.next.is_none_or(|n| n < after) {
            self.gate.next = Some(after);
        }
    }
}

/// Wait for the turn to ask `url`'s host: one request at a time, `min_interval` apart, for
/// every caller in the process. Hold the turn until the answer is in.
pub async fn take_turn(url: &str, min_interval: Duration) -> Turn {
    let host = url::Url::parse(url)
        .ok()
        .map(|u| {
            format!(
                "{}:{}",
                u.host_str().unwrap_or_default(),
                u.port_or_known_default().unwrap_or(0)
            )
        })
        .unwrap_or_default();
    let gate = GATES
        .lock()
        .expect("musicbrainz gates")
        .entry(host)
        .or_insert_with(|| Arc::new(Mutex::new(Gate { next: None })))
        .clone();
    let gate = gate.lock_owned().await;
    if let Some(next) = gate.next {
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        }
    }
    Turn {
        gate,
        min_interval,
        started: Instant::now(),
    }
}

/// How long a 503/429 asks to wait: its Retry-After (seconds), else `fallback`.
pub fn retry_after(headers: &reqwest::header::HeaderMap, fallback: Duration) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(fallback)
        .min(MAX_BACK_OFF)
}

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
        Ok(Self { http, config })
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
            let mut turn = take_turn(url.as_str(), self.config.min_interval).await;
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
                // Everyone in the process waits, not only this caller
                let wait = retry_after(resp.headers(), Duration::from_secs(2 * attempt as u64));
                turn.back_off(wait);
                if attempt >= 3 {
                    return Err(LibraryError::MusicBrainz(format!(
                        "{url}: {status} after {attempt} attempts"
                    )));
                }
                tracing::warn!(%url, %status, ?wait, "musicbrainz asked us to back off");
                drop(turn);
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
    /// Every release MusicBrainz lists it on, so the caller can pick the one the file is from.
    #[serde(default)]
    pub releases: Vec<HitRelease>,
}

/// A release a search hit appears on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HitRelease {
    pub title: String,
    pub mbid: Option<Mbid>,
    /// `Official`, `Bootleg`, `Promotion`… (empty when MusicBrainz doesn't say).
    pub status: String,
    /// The release group's primary and secondary types: `Album`, `Live`, `Compilation`…
    pub kinds: Vec<String>,
}

impl HitRelease {
    /// A bootleg, live recording, compilation, DJ mix or remix album: rarely what a file
    /// tagged with a studio album is from.
    pub fn is_secondary(&self) -> bool {
        self.status.eq_ignore_ascii_case("bootleg")
            || self.kinds.iter().any(|k| {
                ["live", "compilation", "dj-mix", "remix", "mixtape/street"]
                    .contains(&k.to_lowercase().as_str())
            })
    }
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
                    let all = r.get("releases").and_then(Value::as_array);
                    let release = all.and_then(|a| a.first());
                    let releases = all
                        .map(|a| {
                            a.iter()
                                .map(|x| {
                                    let rg = x.get("release-group");
                                    let mut kinds: Vec<String> = rg
                                        .and_then(|g| str_at(g, "primary-type"))
                                        .map(|k| vec![k.to_string()])
                                        .unwrap_or_default();
                                    kinds.extend(
                                        rg.and_then(|g| g.get("secondary-types"))
                                            .and_then(Value::as_array)
                                            .into_iter()
                                            .flatten()
                                            .filter_map(Value::as_str)
                                            .map(str::to_string),
                                    );
                                    HitRelease {
                                        title: str_at(x, "title").unwrap_or_default().to_string(),
                                        mbid: mbid_at(x, "id"),
                                        status: str_at(x, "status").unwrap_or_default().to_string(),
                                        kinds,
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(RecordingHit {
                        mbid: mbid_at(r, "id")?,
                        score: r.get("score").and_then(Value::as_u64).unwrap_or(0) as u32,
                        title: str_at(r, "title").unwrap_or_default().to_string(),
                        artist,
                        release: release.and_then(|x| str_at(x, "title")).map(str::to_string),
                        release_mbid: release.and_then(|x| mbid_at(x, "id")),
                        length_ms: r.get("length").and_then(Value::as_u64),
                        releases,
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
    fn search_hits_keep_every_release_with_its_kind() {
        let v = json!({"recordings": [{
            "id": MB, "score": 100, "title": "Death on Two Legs", "length": 223_000,
            "artist-credit": [{"name": "Queen"}],
            "releases": [
                {"id": MB2, "title": "Ultimate Houston 1977", "status": "Bootleg",
                 "release-group": {"primary-type": "Album", "secondary-types": ["Live"]}},
                {"id": MB, "title": "A Night at the Opera", "status": "Official",
                 "release-group": {"primary-type": "Album"}}
            ]
        }]});
        let h = &parse_search_hits(&v)[0];
        assert_eq!(h.release.as_deref(), Some("Ultimate Houston 1977"));
        assert_eq!(h.releases.len(), 2);
        assert_eq!(h.releases[0].kinds, ["Album", "Live"]);
        assert!(h.releases[0].is_secondary());
        assert_eq!(h.releases[1].status, "Official");
        assert!(!h.releases[1].is_secondary());
    }

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

    /// Two clients (as the node's call sites are) still ask one at a time, a second apart, and a
    /// 503 holds back both.
    #[tokio::test]
    async fn every_client_shares_one_gate_per_host_and_a_503_holds_back_all() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log: Arc<std::sync::Mutex<Vec<Instant>>> = Arc::default();
        let busy = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        {
            let (log, busy, overlapped, served) =
                (log.clone(), busy.clone(), overlapped.clone(), served.clone());
            tokio::spawn(async move {
                loop {
                    let (mut sock, _) = listener.accept().await.unwrap();
                    let (log, busy, overlapped, served) =
                        (log.clone(), busy.clone(), overlapped.clone(), served.clone());
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 2048];
                        let _ = sock.read(&mut buf).await;
                        if busy.fetch_add(1, Ordering::SeqCst) > 0 {
                            overlapped.fetch_add(1, Ordering::SeqCst);
                        }
                        log.lock().unwrap().push(Instant::now());
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let n = served.fetch_add(1, Ordering::SeqCst);
                        let reply = if n == 0 {
                            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string()
                        };
                        busy.fetch_sub(1, Ordering::SeqCst);
                        let _ = sock.write_all(reply.as_bytes()).await;
                    });
                }
            });
        }
        let client = || {
            let mut c = MusicBrainzConfig::new("tests@example.org");
            c.base_url = format!("http://{addr}/ws/2");
            c.min_interval = Duration::from_millis(300);
            MusicBrainzClient::new(c).unwrap()
        };
        let (a, b) = (client(), client());
        let started = Instant::now();
        let (x, y, z) = tokio::join!(
            a.get_json("recording/x", &[]),
            b.get_json("recording/y", &[]),
            b.get_json("recording/z", &[])
        );
        assert!(x.is_ok() && y.is_ok() && z.is_ok(), "{x:?} {y:?} {z:?}");
        assert_eq!(overlapped.load(Ordering::SeqCst), 0, "requests ran in parallel");
        let at = log.lock().unwrap().clone();
        assert_eq!(at.len(), 4, "one 503, then three answers");
        // The first answer was a 503 asking for 2 s: nobody asked before it was over
        assert!(
            at[1] - at[0] >= Duration::from_millis(1_950),
            "{:?}",
            at[1] - at[0]
        );
        for w in at[1..].windows(2) {
            assert!(w[1] - w[0] >= Duration::from_millis(300), "{:?}", w[1] - w[0]);
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn turns_are_spaced_from_their_start_and_a_long_interval_does_not_panic() {
        let url = "http://gate-spacing.invalid:1/ws/2/x";
        let first = take_turn(url, Duration::from_millis(400)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(first);
        let t = Instant::now();
        let mut second = take_turn(url, Duration::from_millis(400)).await;
        assert!(
            t.elapsed() < Duration::from_millis(250),
            "the gap counts from the start of the last request: {:?}",
            t.elapsed()
        );
        second.min_interval = Duration::from_secs(90);
        second.back_off(Duration::from_secs(1));
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
