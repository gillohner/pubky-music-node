//! Minimal client for the hosted Nexus read API (`/v0`), for the social side
//! of a card: which labels, which taggers. Never write to Nexus.
//!
//! Shapes verified against staging on 2026-10-19 and kept defensive (`/v0` is
//! unstable): `GET /v0/resource/by-uri?uri=` and `GET /v0/resource/{id}/tags`
//! both return `{ resource: {id, uri, scheme, indexed_at}, tags: [...] }`,
//! while `GET /v0/stream/resources` uses `details` for the same object; the
//! struct accepts both keys.
//!
//! `POST /v0/stream/resources/by_ids` with `uris` is on a branch not yet
//! deployed (handoff from the Pinakes/Nexus work); staging answers 404, which
//! this client reports as [`NexusError::RouteUnavailable`].

use crate::config::PmPubkyConfig;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum NexusError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("not indexed")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("rate limited; retry after {0:?}")]
    RateLimited(Option<Duration>),
    #[error("route not available on this Nexus")]
    RouteUnavailable,
    #[error("Nexus returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceDetails {
    /// 32 lowercase hex chars: Nexus's own id. Join with Pinakes by `uri`.
    pub id: String,
    pub uri: String,
    #[serde(default)]
    pub scheme: Option<String>,
    /// Unix milliseconds.
    #[serde(default)]
    pub indexed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagView {
    pub label: String,
    #[serde(default)]
    pub taggers: Vec<String>,
    #[serde(default)]
    pub taggers_count: u64,
    #[serde(default)]
    pub relationship: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceView {
    #[serde(alias = "details")]
    pub resource: ResourceDetails,
    #[serde(default)]
    pub tags: Vec<TagView>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

impl ResourceView {
    pub fn taggers_for(&self, label: &str) -> Option<&TagView> {
        self.tags.iter().find(|t| t.label == label)
    }

    /// Distinct taggers across all labels, from the (paged) tagger lists.
    pub fn distinct_taggers(&self) -> usize {
        let mut v: Vec<&str> = self
            .tags
            .iter()
            .flat_map(|t| t.taggers.iter().map(String::as_str))
            .collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Taggers {
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub relationship: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HotTag {
    pub label: String,
    #[serde(default)]
    pub taggers_id: Vec<String>,
    #[serde(default)]
    pub tagged_count: u64,
    #[serde(default)]
    pub taggers_count: u64,
}

#[derive(Clone)]
pub struct NexusClient {
    http: reqwest::Client,
    base: String,
}

impl NexusClient {
    pub fn new(config: &PmPubkyConfig) -> Result<Self, NexusError> {
        let http = reqwest::Client::builder()
            .user_agent(config.user_agent())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base: config.nexus_base.trim_end_matches('/').to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v0{path}", self.base)
    }

    async fn json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, NexusError> {
        let status = resp.status();
        let is_json = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("application/json"));
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp.text().await?;
        match status {
            s if s.is_success() => serde_json::from_str(&body).map_err(|e| {
                NexusError::Decode(format!("{e}: {}", body.chars().take(200).collect::<String>()))
            }),
            StatusCode::NOT_FOUND if is_json => Err(NexusError::NotFound),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => Err(NexusError::RouteUnavailable),
            StatusCode::BAD_REQUEST => Err(NexusError::BadRequest(error_message(&body))),
            StatusCode::TOO_MANY_REQUESTS => Err(NexusError::RateLimited(retry_after)),
            s => Err(NexusError::Status {
                status: s.as_u16(),
                body: error_message(&body),
            }),
        }
    }

    /// The resource and its tags for a raw URI (Nexus normalizes it).
    pub async fn resource_by_uri(&self, uri: &str) -> Result<ResourceView, NexusError> {
        let resp = self
            .http
            .get(with_query(&self.url("/resource/by-uri"), &[("uri", uri)]))
            .send()
            .await?;
        Self::json(resp).await
    }

    pub async fn resource_tags(&self, resource_id: &str) -> Result<ResourceView, NexusError> {
        let resp = self
            .http
            .get(self.url(&format!("/resource/{resource_id}/tags")))
            .send()
            .await?;
        Self::json(resp).await
    }

    pub async fn resource_taggers(
        &self,
        resource_id: &str,
        label: &str,
        limit: Option<u32>,
    ) -> Result<Taggers, NexusError> {
        let mut url = self.url(&format!("/resource/{resource_id}/tags/{label}/taggers"));
        if let Some(l) = limit {
            url = with_query(&url, &[("limit", &l.clamp(1, 100).to_string())]);
        }
        Self::json(self.http.get(url).send().await?).await
    }

    /// Tag labels that start with `prefix` (`/v0/search/tags/by_prefix/{prefix}`).
    pub async fn tags_by_prefix(&self, prefix: &str, limit: u32) -> Result<Vec<String>, NexusError> {
        let p: String = url::form_urlencoded::byte_serialize(prefix.as_bytes()).collect();
        let url = with_query(
            &self.url(&format!("/search/tags/by_prefix/{p}")),
            &[("limit", &limit.clamp(1, 40).to_string())],
        );
        Self::json(self.http.get(url).send().await?).await
    }

    pub async fn tags_hot(&self, limit: Option<u32>) -> Result<Vec<HotTag>, NexusError> {
        let mut url = self.url("/tags/hot");
        if let Some(l) = limit {
            url = with_query(&url, &[("limit", &l.clamp(1, 40).to_string())]);
        }
        Self::json(self.http.get(url).send().await?).await
    }

    /// Batch hydration by URI (1–100). Returns [`NexusError::RouteUnavailable`]
    /// until the `feat/stream-resources-by-ids` branch is deployed.
    pub async fn resources_by_uris(
        &self,
        uris: &[String],
        viewer_id: Option<&str>,
    ) -> Result<Vec<ResourceView>, NexusError> {
        if uris.is_empty() || uris.len() > 100 {
            return Err(NexusError::BadRequest("uris must hold 1–100 entries".into()));
        }
        let mut body = serde_json::json!({ "uris": uris });
        if let Some(v) = viewer_id {
            body["viewer_id"] = v.into();
        }
        let resp = self
            .http
            .post(self.url("/stream/resources/by_ids"))
            .json(&body)
            .send()
            .await?;
        Self::json(resp).await
    }
}

/// Append query pairs, percent-encoded (reqwest's `query` needs a feature the
/// workspace does not enable).
pub(crate) fn with_query(url: &str, pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    let q = s.finish();
    if q.is_empty() {
        url.to_string()
    } else {
        format!("{url}?{q}")
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
    fn accepts_both_resource_keys() {
        let a: ResourceView = serde_json::from_str(r#"{"resource":{"id":"a","uri":"https://x/"},"tags":[{"label":"music","taggers":["u1","u2"],"taggers_count":2,"relationship":false},{"label":"video","taggers":["u2"],"taggers_count":1}]}"#).unwrap();
        let b: ResourceView = serde_json::from_str(
            r#"{"details":{"id":"a","uri":"https://x/","scheme":"https","indexed_at":1},"tags":[]}"#,
        )
        .unwrap();
        assert_eq!(a.resource.id, b.resource.id);
        assert_eq!(a.taggers_for("music").unwrap().taggers_count, 2);
        assert_eq!(a.distinct_taggers(), 2);
    }
}
