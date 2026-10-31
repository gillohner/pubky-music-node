//! Read-only client for the Pinakes card API (`/v1`), typed against
//! `docs/pinakes-openapi.json` (Pinakes 0.1.0, exported 2026-10-19).
//!
//! Facts from §12 and the live instance:
//! * no auth, CORS open; per-IP budget 120 requests/min, `POST /v1/resolve` 30/min;
//! * keyset `cursor` + `limit` (max 500), no offsets; cursors die on restart;
//! * no push: poll `POST /v1/changes/lookup` for the links you hold;
//! * there is **no** `GET /v1/resources/by-uri` route (the context file mentions
//!   one; the live daemon answers `400 invalid resource ID` because it matches
//!   `/v1/resources/{id}`). URIs go through `POST /v1/resources/lookup`;
//! * `POST /v1/changes/lookup` takes `resource_ids`, not URIs;
//! * `facet` accepts only `tags`;
//! * join with Nexus by **URL**, never by id;
//! * text is plain text: escape before rendering as HTML.
//!
//! Music typing (`MusicRecording`, `isrc`, `mbid`, `also_on`, …) is Pinakes
//! step 8, partly live already (Spotify and Deezer tracks come back as
//! `MusicRecording`). The optional fields on [`Card`] are there for it and
//! stay `None` until the daemon emits them.

use crate::config::PmPubkyConfig;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_LIMIT: usize = 500;
pub const LOOKUP_CHUNK: usize = 500;
const BUDGET_PER_MINUTE: usize = 120;

#[derive(Debug, thiserror::Error)]
pub enum PinakesError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited { retry_after: Option<Duration> },
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("cursor expired (410); restart pagination")]
    CursorGone,
    #[error("pending: the resource is being resolved (202)")]
    Pending(Box<ResourceDetail>),
    #[error("Pinakes returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
}

/// Geographic point, present on place and event cards only.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub lat: f64,
    pub lon: f64,
}

/// Hint that the same content exists on other services (never a merge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupRef {
    pub id: String,
    pub size: i64,
}

/// Step 8: the same recording or release on another service, found through
/// exact MusicBrainz / Wikidata ids and marked `derived` with its source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlsoOn {
    /// Pinakes resource id of the other link (music step).
    #[serde(default)]
    pub id: Option<String>,
    pub uri: String,
    #[serde(default)]
    pub site: Option<String>,
    /// `true` when Pinakes derived the link (MusicBrainz, Wikidata, iTunes,
    /// Deezer) rather than someone on Pubky tagging it; derived links can be
    /// unavailable in some countries, which is why tagged links sort first.
    #[serde(default)]
    pub derived: Option<bool>,
    #[serde(default)]
    pub source: Option<String>,
    /// Where a derived link came from (`musicbrainz`, `wikidata`, …).
    #[serde(default)]
    pub via: Option<String>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

impl AlsoOn {
    /// Tagged by someone on Pubky (has its own card), not derived.
    pub fn is_tagged(&self) -> bool {
        self.id.is_some() && self.derived != Some(true)
    }
}

/// The one card every route returns. Absent values are `None`, never `""`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub uri: String,
    /// `ok`, `pending`, `unreachable`, `missing`, `error`.
    pub status: String,
    /// A schema.org type such as `MusicRecording`, `VideoObject`, `Event`.
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub point: Option<Point>,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default)]
    pub group: Option<GroupRef>,
    /// Licence credit to show under the card when present.
    #[serde(default)]
    pub attribution: Option<String>,
    #[serde(default)]
    pub event: Option<Value>,
    #[serde(default)]
    pub occurrence: Option<Value>,
    // ---- Music facts: land with Pinakes step 8, `None` until then ----
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default)]
    pub upc: Option<String>,
    #[serde(default)]
    pub mbid: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    /// Seconds or ISO 8601, whichever step 8 settles on; kept as a value.
    #[serde(default)]
    pub duration: Option<Value>,
    #[serde(default)]
    pub also_on: Vec<AlsoOn>,
    /// Events: up to 3 listings that are possibly the same event (a hint,
    /// never a merge).
    #[serde(default)]
    pub maybe_same: Vec<MaybeSame>,
    /// Any field this client does not know yet, preserved.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// Same-event hint on event and occurrence cards: starts within 300 s,
/// within 500 m, similar name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaybeSame {
    pub id: String,
    pub uri: String,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
}

/// `details.facts` of a music card (music step): exact identities and the
/// few catalogue facts Pinakes carries. Everything else comes from
/// MusicBrainz by `mbid` / `isrc`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Facts {
    #[serde(default)]
    pub mbid: Option<String>,
    /// Up to 8 ISRCs, `; `-separated as Pinakes sends them; see [`Facts::isrcs`].
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default)]
    pub upc: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    /// `Group` or `Person` for artists.
    #[serde(default)]
    pub artist_type: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    /// The album the recording first appeared on (not a compilation).
    #[serde(default)]
    pub original_album: Option<String>,
    /// Seconds, sent as a string or a number.
    #[serde(default)]
    pub duration: Option<Value>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

impl Facts {
    pub fn isrcs(&self) -> Vec<String> {
        self.isrc
            .as_deref()
            .unwrap_or("")
            .split([';', ','])
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect()
    }

    pub fn duration_secs(&self) -> Option<u64> {
        match &self.duration {
            Some(Value::Number(n)) => n.as_f64().map(|f| f.round() as u64),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f.round() as u64),
            _ => None,
        }
    }
}

impl Card {
    /// The artist as the card describes it: the facts when the card carries
    /// them, else the first segment of Pinakes's description
    /// ("Queen · A Night At The Opera · 5:55").
    pub fn artist_hint(&self) -> Option<String> {
        if let Some(a) = self.artist.as_ref().filter(|a| !a.trim().is_empty()) {
            return Some(a.clone());
        }
        if !self.is_music() || self.type_.as_deref() == Some("MusicGroup") {
            return None;
        }
        let d = self.description.as_deref()?.replace('\u{a0}', " ");
        let sep = if d.contains(" · ") {
            " · "
        } else if d.contains(" - ") {
            " - "
        } else {
            return None;
        };
        let first = d.split(sep).next().map(str::trim)?;
        let sentence = first.contains(" by ") || first.contains(" on ") || first.ends_with('.');
        (!first.is_empty() && first.len() <= 60 && !sentence).then(|| first.to_string())
    }

    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    pub fn is_music(&self) -> bool {
        matches!(
            self.type_.as_deref(),
            Some("MusicRecording" | "MusicAlbum" | "MusicGroup" | "MusicComposition" | "MusicRelease")
        )
    }
}

/// `details` on the full read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Details {
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub address: Option<Value>,
    #[serde(default)]
    pub bbox: Option<Vec<f64>>,
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub all_day: Option<bool>,
    #[serde(default)]
    pub event_status: Option<String>,
    #[serde(default)]
    pub attendance: Option<String>,
    #[serde(default)]
    pub venue: Option<Value>,
    #[serde(default)]
    pub organizer: Option<Value>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Music step: exact identities and catalogue facts.
    #[serde(default)]
    pub facts: Option<Facts>,
    /// Every other link of the group, not only the one-per-service list on
    /// the card.
    #[serde(default)]
    pub also_on: Vec<AlsoOn>,
    /// Events: up to 10 possibly-same listings.
    #[serde(default)]
    pub maybe_same: Vec<MaybeSame>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `GET /v1/resources/{id}` and `POST /v1/resolve`: the card plus details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceDetail {
    #[serde(flatten)]
    pub card: Card,
    #[serde(default)]
    pub details: Details,
}

impl ResourceDetail {
    pub fn facts(&self) -> Facts {
        self.details.facts.clone().unwrap_or_default()
    }

    pub fn mbid(&self) -> Option<String> {
        self.details
            .facts
            .as_ref()
            .and_then(|f| f.mbid.clone())
            .or_else(|| self.card.mbid.clone())
    }

    pub fn isrcs(&self) -> Vec<String> {
        let mut v = self.facts().isrcs();
        if v.is_empty() {
            v.extend(self.card.isrc.iter().map(|s| s.to_ascii_uppercase()));
        }
        v
    }

    pub fn upc(&self) -> Option<String> {
        self.details
            .facts
            .as_ref()
            .and_then(|f| f.upc.clone())
            .or_else(|| self.card.upc.clone())
    }

    /// The full `also_on` list: details first, else the card's.
    pub fn also_on(&self) -> &[AlsoOn] {
        if self.details.also_on.is_empty() {
            &self.card.also_on
        } else {
            &self.details.also_on
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagFacet {
    pub label: String,
    pub count: i64,
}

/// One count of `facet=type`: a card type and how many matched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeFacet {
    #[serde(rename = "type")]
    pub type_: String,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Facets {
    #[serde(default)]
    pub tags: Vec<TagFacet>,
    /// `facet=type`: counts by card type.
    #[serde(default, rename = "type")]
    pub types: Vec<TypeFacet>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub items: Vec<Card>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub facets: Option<Facets>,
    #[serde(default)]
    pub reach_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeLine {
    pub seq: i64,
    pub resource_id: String,
    /// Unix milliseconds.
    pub at: i64,
    /// `created`, `updated`, `aliased`, …
    pub class: String,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub occurrence: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangePage {
    pub lines: Vec<ChangeLine>,
    pub next_cursor: i64,
    pub has_more: bool,
    #[serde(default)]
    pub oldest_cursor: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMember {
    pub id: String,
    pub uri: String,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupDetail {
    pub id: String,
    #[serde(default)]
    pub best: Option<Card>,
    #[serde(default)]
    pub members: Vec<GroupMember>,
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    /// Exact keys the group joins on (`hub:mb:artist:<mbid>`, `hub:wikidata:Q…`).
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub curated: Option<bool>,
}

/// A public collection post that holds a link (`/v1/resources/{id}/collections`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CollectionRef {
    /// The post's `pubky://` URI.
    pub uri: String,
    /// The author's public key (z32).
    #[serde(default)]
    pub author: String,
}

/// One page of [`CollectionRef`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CollectionsPage {
    #[serde(default, alias = "collections")]
    pub items: Vec<CollectionRef>,
    /// How many hold it in all, when Pinakes says (else the page's length).
    #[serde(default, alias = "count")]
    pub total: Option<u64>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoCounts {
    pub resources: i64,
    pub references: i64,
    pub occurrences: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    #[serde(default)]
    pub version: Option<String>,
    pub counts: InfoCounts,
    #[serde(default)]
    pub ingestion: Vec<Value>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// `GET /v1/search` parameters. `None` fields are not sent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    /// schema.org type; ancestors match (`Event` matches `MusicEvent`).
    pub type_: Option<String>,
    /// Up to 5 labels, comma separated.
    pub tags: Option<String>,
    /// `all` to require every tag (at most 3), default any.
    pub match_: Option<String>,
    pub author: Option<String>,
    /// `following` with `viewer`.
    pub reach: Option<String>,
    pub viewer: Option<String>,
    pub scheme: Option<String>,
    pub host: Option<String>,
    /// `activity`, `recent`, `distance` or `start`; `trending` with `window`
    /// arrives with Pinakes's music step (answers §3).
    pub sort: Option<String>,
    /// With `sort=trending`: `1h`–`90d`, e.g. `7d`. Ranks by tags made inside
    /// the window, not by event dates.
    pub window: Option<String>,
    /// Exact-id search across services: `mbid:<uuid>`, `isrc:<ISRC>`,
    /// `upc:<digits>` (music step, answers §3).
    pub key: Option<String>,
    /// RFC 3339, Unix ms or `now-7d`. Bounds the card's event `start`, so it
    /// excludes every card without a date (all music cards). Not a
    /// reference-time window.
    pub from: Option<String>,
    pub to: Option<String>,
    /// `tags` today; `type` from the next core release (answers §2).
    pub facet: Option<String>,
    /// `lat,lon` (needs `radius`, in metres).
    pub near: Option<String>,
    /// A resource id: rank what its taggers also referenced (when Pinakes ships it).
    pub related: Option<String>,
    pub radius: Option<String>,
    /// `group` for one row per group.
    pub collapse: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl SearchQuery {
    pub fn tags(tags: &str) -> Self {
        Self {
            tags: Some(tags.to_string()),
            ..Default::default()
        }
    }

    /// Every tagged link carrying an exact id: `mbid:<uuid>`, `isrc:<code>`,
    /// `upc:<barcode>`, `wikidata:<Q-id>`.
    pub fn key(key: &str) -> Self {
        Self {
            key: Some(key.to_string()),
            ..Default::default()
        }
    }

    /// A chart: `type_` (`MusicRecording` or `MusicAlbum`) ranked by tags made
    /// inside `window` (`24h`, `7d`, `30d`), one song across services one row.
    pub fn trending(type_: &str, window: &str) -> Self {
        Self {
            type_: Some(type_.to_string()),
            sort: Some("trending".to_string()),
            window: Some(window.to_string()),
            collapse: Some("group".to_string()),
            ..Default::default()
        }
    }

    pub(crate) fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        let mut push = |k: &'static str, o: &Option<String>| {
            if let Some(x) = o {
                v.push((k, x.clone()));
            }
        };
        push("q", &self.q);
        push("type", &self.type_);
        push("tags", &self.tags);
        push("match", &self.match_);
        push("author", &self.author);
        push("reach", &self.reach);
        push("viewer", &self.viewer);
        push("scheme", &self.scheme);
        push("host", &self.host);
        push("sort", &self.sort);
        push("window", &self.window);
        push("key", &self.key);
        push("from", &self.from);
        push("to", &self.to);
        push("facet", &self.facet);
        push("near", &self.near);
        push("related", &self.related);
        push("radius", &self.radius);
        push("collapse", &self.collapse);
        push("cursor", &self.cursor);
        if let Some(l) = self.limit {
            v.push(("limit", l.clamp(1, MAX_LIMIT).to_string()));
        }
        v
    }
}

/// Client-side share of the per-IP budget: a sliding one-minute window.
#[derive(Clone)]
struct Budget {
    stamps: Arc<Mutex<VecDeque<Instant>>>,
    per_minute: usize,
}

impl Budget {
    fn new(per_minute: usize) -> Self {
        Self {
            stamps: Arc::new(Mutex::new(VecDeque::new())),
            per_minute,
        }
    }

    /// Wait until a request fits in the window, then record it.
    async fn acquire(&self) {
        loop {
            let wait = {
                let mut s = self.stamps.lock().expect("budget poisoned");
                let now = Instant::now();
                while s
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
                {
                    s.pop_front();
                }
                if s.len() < self.per_minute {
                    s.push_back(now);
                    None
                } else {
                    s.front()
                        .map(|t| Duration::from_secs(60).saturating_sub(now.duration_since(*t)))
                }
            };
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d.max(Duration::from_millis(10))).await,
            }
        }
    }
}

#[derive(Clone)]
pub struct PinakesClient {
    http: reqwest::Client,
    base: String,
    budget: Budget,
}

impl PinakesClient {
    pub fn new(config: &PmPubkyConfig) -> Result<Self, PinakesError> {
        let http = reqwest::Client::builder()
            .user_agent(config.user_agent())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base: config.pinakes_base.trim_end_matches('/').to_string(),
            budget: Budget::new(BUDGET_PER_MINUTE),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1{path}", self.base)
    }

    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, PinakesError> {
        self.budget.acquire().await;
        let resp = build().send().await?;
        if resp.status() != StatusCode::TOO_MANY_REQUESTS {
            return Ok(resp);
        }
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);
        tracing::warn!(?retry_after, "pinakes rate limited; retrying once");
        tokio::time::sleep(
            retry_after
                .unwrap_or(Duration::from_secs(2))
                .min(Duration::from_secs(30)),
        )
        .await;
        self.budget.acquire().await;
        let resp = build().send().await?;
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(PinakesError::RateLimited { retry_after });
        }
        Ok(resp)
    }

    async fn json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, PinakesError> {
        let status = resp.status();
        let body = resp.text().await?;
        match status {
            s if s.is_success() => serde_json::from_str(&body).map_err(|e| {
                PinakesError::Decode(format!("{e}: {}", body.chars().take(200).collect::<String>()))
            }),
            StatusCode::NOT_FOUND => Err(PinakesError::NotFound),
            StatusCode::BAD_REQUEST => Err(PinakesError::BadRequest(error_message(&body))),
            StatusCode::GONE => Err(PinakesError::CursorGone),
            s => Err(PinakesError::Status {
                status: s.as_u16(),
                body: error_message(&body),
            }),
        }
    }

    /// `/v1/info` as JSON, for what this instance ships (limits name its
    /// optional features) without depending on every field.
    pub async fn info_raw(&self) -> Result<Value, PinakesError> {
        let url = self.url("/info");
        let resp = self.send(|| self.http.get(&url)).await?;
        Self::json(resp).await
    }

    pub async fn info(&self) -> Result<Info, PinakesError> {
        let url = self.url("/info");
        let resp = self.send(|| self.http.get(&url)).await?;
        Self::json(resp).await
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<SearchResponse, PinakesError> {
        let pairs = query.to_pairs();
        let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let url = crate::nexus::with_query(&self.url("/search"), &borrowed);
        let resp = self.send(|| self.http.get(&url)).await?;
        Self::json(resp).await
    }

    /// Cards for links you hold. Unknown links are simply absent from the
    /// result; order is not guaranteed. More than 500 URIs are chunked.
    pub async fn lookup(&self, uris: &[String]) -> Result<Vec<Card>, PinakesError> {
        let mut out = Vec::new();
        for chunk in uris.chunks(LOOKUP_CHUNK) {
            let url = self.url("/resources/lookup");
            let body = serde_json::json!({ "uris": chunk });
            let resp = self.send(|| self.http.post(&url).json(&body)).await?;
            let cards: Vec<Card> = Self::json(resp).await?;
            out.extend(cards);
        }
        Ok(out)
    }

    pub async fn lookup_ids(&self, ids: &[String]) -> Result<Vec<Card>, PinakesError> {
        let mut out = Vec::new();
        for chunk in ids.chunks(LOOKUP_CHUNK) {
            let url = self.url("/resources/lookup");
            let body = serde_json::json!({ "ids": chunk });
            let resp = self.send(|| self.http.post(&url).json(&body)).await?;
            let cards: Vec<Card> = Self::json(resp).await?;
            out.extend(cards);
        }
        Ok(out)
    }

    /// One card for one URI, or `None` if Pinakes has not seen it.
    pub async fn card_for_uri(&self, uri: &str) -> Result<Option<Card>, PinakesError> {
        Ok(self.lookup(&[uri.to_string()]).await?.into_iter().next())
    }

    /// Full read for one URI (any spelling) via `GET /v1/resources/by-uri`,
    /// which ships in the next core release (answers §2). On the previous
    /// version the path matches `/resources/{id}` and answers 400, so this
    /// falls back to `lookup` + `resource(id)`. `None` when Pinakes has not
    /// seen the link; never fetches.
    pub async fn resource_by_uri(&self, uri: &str) -> Result<Option<ResourceDetail>, PinakesError> {
        let url = crate::nexus::with_query(&self.url("/resources/by-uri"), &[("uri", uri)]);
        let resp = self.send(|| self.http.get(&url)).await?;
        match Self::json::<ResourceDetail>(resp).await {
            Ok(d) => Ok(Some(d)),
            Err(PinakesError::NotFound) => Ok(None),
            Err(PinakesError::BadRequest(_)) => {
                tracing::debug!("by-uri route not on this Pinakes yet; falling back to lookup");
                match self.card_for_uri(uri).await? {
                    Some(card) => self.resource(&card.id).await.map(Some),
                    None => Ok(None),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The public collections that hold a resource's links (`GET /v1/resources/{id}/collections`,
    /// Pinakes TASK-in-collections): newest first, the viewer's own first when `viewer` is given.
    /// A bare list is taken as the page too.
    pub async fn resource_collections(
        &self,
        id: &str,
        viewer: Option<&str>,
        limit: usize,
    ) -> Result<CollectionsPage, PinakesError> {
        let lim = limit.to_string();
        let mut q = vec![("limit", lim.as_str())];
        if let Some(v) = viewer {
            q.push(("viewer", v));
        }
        let url = crate::nexus::with_query(&self.url(&format!("/resources/{id}/collections")), &q);
        let resp = self.send(|| self.http.get(&url)).await?;
        let v: Value = Self::json(resp).await?;
        match v {
            Value::Array(a) => Ok(CollectionsPage {
                items: serde_json::from_value(Value::Array(a)).unwrap_or_default(),
                ..Default::default()
            }),
            v => serde_json::from_value(v).map_err(|e| PinakesError::Decode(e.to_string())),
        }
    }

    /// Full read with `details`, by Pinakes id.
    pub async fn resource(&self, id: &str) -> Result<ResourceDetail, PinakesError> {
        let url = self.url(&format!("/resources/{id}"));
        let resp = self.send(|| self.http.get(&url)).await?;
        Self::json(resp).await
    }

    /// What changed on resources you hold since `since` (`now-1d`, RFC 3339,
    /// or Unix ms as text). Takes Pinakes resource ids; see
    /// [`changes_for_uris`](Self::changes_for_uris) for URIs.
    pub async fn changes_lookup(
        &self,
        resource_ids: &[String],
        since: &str,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<ChangePage, PinakesError> {
        let url = self.url("/changes/lookup");
        let mut body = serde_json::json!({ "resource_ids": resource_ids, "since": since });
        if let Some(c) = cursor {
            body["cursor"] = c.into();
        }
        if let Some(l) = limit {
            body["limit"] = (l.min(MAX_LIMIT)).into();
        }
        let resp = self.send(|| self.http.post(&url).json(&body)).await?;
        Self::json(resp).await
    }

    /// `POST /v1/changes/lookup` with `{"uris": […]}` and an optional `since`,
    /// the shape of the next core release (answers §2). On the previous
    /// version the body is rejected (400) and this falls back to
    /// [`changes_for_uris`](Self::changes_for_uris) with `since` defaulting
    /// to `now-30d`.
    pub async fn changes_lookup_uris(
        &self,
        uris: &[String],
        since: Option<&str>,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<ChangePage, PinakesError> {
        let url = self.url("/changes/lookup");
        let mut body = serde_json::json!({ "uris": uris });
        if let Some(s) = since {
            body["since"] = s.into();
        }
        if let Some(c) = cursor {
            body["cursor"] = c.into();
        }
        if let Some(l) = limit {
            body["limit"] = (l.min(MAX_LIMIT)).into();
        }
        let resp = self.send(|| self.http.post(&url).json(&body)).await?;
        match Self::json(resp).await {
            Err(PinakesError::BadRequest(_)) => {
                tracing::debug!("uris body not accepted by this Pinakes yet; falling back to ids");
                self.changes_for_uris(uris, since.unwrap_or("now-30d")).await
            }
            other => other,
        }
    }

    /// Changes for URIs: one lookup to map URIs to ids, then the changes.
    pub async fn changes_for_uris(&self, uris: &[String], since: &str) -> Result<ChangePage, PinakesError> {
        let ids: Vec<String> = self.lookup(uris).await?.into_iter().map(|c| c.id).collect();
        if ids.is_empty() {
            return Ok(ChangePage {
                lines: vec![],
                next_cursor: 0,
                has_more: false,
                oldest_cursor: 0,
            });
        }
        self.changes_lookup(&ids, since, None, None).await
    }

    pub async fn group(&self, id: &str) -> Result<GroupDetail, PinakesError> {
        let url = self.url(&format!("/groups/{id}"));
        let resp = self.send(|| self.http.get(&url)).await?;
        Self::json(resp).await
    }

    /// Compose-time preview for a link nobody tagged yet. Synchronous for a
    /// few seconds, then `202` with a pending card, surfaced as
    /// [`PinakesError::Pending`]. Budget: 30/min per IP.
    pub async fn resolve(&self, uri: &str) -> Result<ResourceDetail, PinakesError> {
        let url = self.url("/resolve");
        let body = serde_json::json!({ "uri": uri });
        let resp = self.send(|| self.http.post(&url).json(&body)).await?;
        if resp.status() == StatusCode::ACCEPTED {
            let pending: ResourceDetail = Self::json(resp).await?;
            return Err(PinakesError::Pending(Box::new(pending)));
        }
        Self::json(resp).await
    }
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_pairs_rename_and_clamp() {
        let q = SearchQuery {
            type_: Some("MusicRecording".into()),
            match_: Some("all".into()),
            limit: Some(9999),
            ..SearchQuery::tags("music")
        };
        let pairs = q.to_pairs();
        assert!(pairs.contains(&("type", "MusicRecording".into())));
        assert!(pairs.contains(&("match", "all".into())));
        assert!(pairs.contains(&("limit", "500".into())));
        assert!(!pairs.iter().any(|(k, _)| *k == "q"));
    }

    #[test]
    fn error_message_extracts_json_error() {
        assert_eq!(
            error_message(r#"{"error":"facet must be tags"}"#),
            "facet must be tags"
        );
        assert_eq!(error_message("plain"), "plain");
    }

    #[tokio::test]
    async fn budget_window_admits_up_to_limit_without_waiting() {
        let b = Budget::new(3);
        let t = Instant::now();
        b.acquire().await;
        b.acquire().await;
        b.acquire().await;
        assert!(t.elapsed() < Duration::from_millis(50));
    }
}
