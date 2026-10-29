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

/// A Pubky profile as Nexus indexes it (the person's own `/pub/pubky.app/profile.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UserDetails {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub bio: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// A `pubky://…/files/…` link; the avatar itself is served at `/static/avatar/{id}`.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub links: Vec<UserLink>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UserLink {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UserCounts {
    #[serde(default)]
    pub tagged: u64,
    #[serde(default)]
    pub following: u64,
    #[serde(default)]
    pub followers: u64,
    #[serde(default)]
    pub collections: u64,
}

/// How the viewer and this person relate (only with `viewer_id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Relationship {
    #[serde(default)]
    pub following: bool,
    #[serde(default)]
    pub followed_by: bool,
}

/// A post as Nexus lists it; only what search shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PostView {
    pub details: PostDetails,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PostDetails {
    pub id: String,
    pub author: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UserView {
    pub details: UserDetails,
    #[serde(default)]
    pub counts: UserCounts,
    #[serde(default)]
    pub relationship: Option<Relationship>,
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

    /// Profiles in one call (`POST /v0/stream/users/by_ids`); people Nexus doesn't know are
    /// left out.
    pub async fn users_by_ids(
        &self,
        ids: &[String],
        viewer: Option<&str>,
    ) -> Result<Vec<UserView>, NexusError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut body = serde_json::json!({ "user_ids": ids });
        if let Some(v) = viewer {
            body["viewer_id"] = Value::String(v.to_string());
        }
        let resp = self
            .http
            .post(self.url("/stream/users/by_ids"))
            .json(&body)
            .send()
            .await?;
        Self::json(resp).await
    }

    /// People whose name starts with `username` (`GET /v0/stream/users/username`), as profiles.
    pub async fn users_by_name(&self, username: &str, limit: u32) -> Result<Vec<UserView>, NexusError> {
        let resp = self
            .http
            .get(self.url("/stream/users/username"))
            .query(&[("username", username), ("limit", &limit.to_string())])
            .send()
            .await?;
        Self::json(resp).await
    }

    /// Posts whose text matches `q` (`GET /v0/search/posts/by_content`): their keys, best first.
    pub async fn posts_by_content(&self, q: &str, limit: u32) -> Result<Vec<String>, NexusError> {
        #[derive(Deserialize)]
        struct Hit {
            post_key: String,
        }
        let resp = self
            .http
            .get(self.url("/search/posts/by_content"))
            .query(&[("q", q), ("limit", &limit.to_string())])
            .send()
            .await?;
        let hits: Vec<Hit> = Self::json(resp).await?;
        Ok(hits.into_iter().map(|h| h.post_key).collect())
    }

    /// Posts carrying a tag (`GET /v0/search/posts/by_tag/{label}`): their keys. Nexus answers
    /// either keys or `{post_key, …}` objects, depending on its version.
    pub async fn posts_by_tag(&self, label: &str, limit: u32) -> Result<Vec<String>, NexusError> {
        let resp = self
            .http
            .get(self.url(&format!("/search/posts/by_tag/{label}")))
            .query(&[("limit", &limit.to_string())])
            .send()
            .await?;
        let raw: Vec<Value> = Self::json(resp).await?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                Value::Object(o) => o.get("post_key").and_then(|k| k.as_str()).map(str::to_string),
                _ => None,
            })
            .collect())
    }

    /// Posts by key (`author:id`), in one call (`POST /v0/stream/posts/by_ids`).
    pub async fn posts_by_ids(&self, keys: &[String]) -> Result<Vec<PostView>, NexusError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let resp = self
            .http
            .post(self.url("/stream/posts/by_ids"))
            .json(&serde_json::json!({ "post_ids": keys }))
            .send()
            .await?;
        Self::json(resp).await
    }

    /// One profile, with the viewer's relationship when given.
    pub async fn user(&self, id: &str, viewer: Option<&str>) -> Result<UserView, NexusError> {
        let mut url = self.url(&format!("/user/{id}"));
        if let Some(v) = viewer {
            url = with_query(&url, &[("viewer_id", v)]);
        }
        Self::json(self.http.get(url).send().await?).await
    }

    /// A person's avatar bytes and media type (`/static/avatar/{id}`, outside `/v0`).
    pub async fn avatar(&self, id: &str) -> Result<(Vec<u8>, String), NexusError> {
        let resp = self
            .http
            .get(format!("{}/static/avatar/{id}", self.base))
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(NexusError::NotFound);
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = resp.error_for_status()?.bytes().await?.to_vec();
        Ok((bytes, ct))
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
