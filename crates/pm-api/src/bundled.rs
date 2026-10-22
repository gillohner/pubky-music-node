//! The Pubky homeserver bundled with the node (STEWARD-BUNDLED-HOMESERVER.md): the compose and
//! Umbrel installs in `deploy/bundle` run pubky-homeserver next to the node, and the node manages
//! it through the homeserver's admin API (`X-Admin-Password`), reachable only on the compose
//! network. The password stays in the node's config or environment: it is never logged, never
//! returned by the API, and never sent anywhere but the admin endpoint.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Where the bundled homeserver's admin API is, and its password. `Debug` never shows the password.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AdminEndpoint {
    /// e.g. `http://homeserver:6288` on the compose network.
    pub url: String,
    pub password: String,
}

impl std::fmt::Debug for AdminEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminEndpoint")
            .field("url", &self.url)
            .field("password", &"<set>")
            .finish()
    }
}

impl AdminEndpoint {
    /// Configured when both the address and the password are set and the address is http(s).
    pub fn new(url: &str, password: &str) -> Option<Self> {
        let url = url.trim().trim_end_matches('/');
        let ok = url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
        (ok && !password.is_empty()).then(|| Self {
            url: url.to_string(),
            password: password.to_string(),
        })
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn endpoint(s: &NodeState) -> Result<AdminEndpoint, ApiError> {
    s.bundled_homeserver().cloned().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "this node has no bundled homeserver (set [homeserver] admin_url and admin_password)",
        )
    })
}

fn unreachable(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("the bundled homeserver: {e}"))
}

/// A call to the admin API; JSON answers are parsed, text ones (a minted token) come back as a
/// JSON string.
pub(crate) async fn admin(
    s: &NodeState,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, ApiError> {
    let ep = endpoint(s)?;
    let mut rb = s
        .http_client()
        .request(method, format!("{}{path}", ep.url))
        .header("X-Admin-Password", &ep.password)
        .timeout(TIMEOUT);
    if let Some(b) = body {
        rb = rb.json(b);
    }
    let resp = rb.send().await.map_err(|e| unreachable(e.without_url()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| unreachable(e.without_url()))?;
    if status == StatusCode::UNAUTHORIZED {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "the bundled homeserver refused the node's admin password: check PM_HOMESERVER_ADMIN_PASSWORD against the homeserver's [admin] admin_password",
        ));
    }
    if !status.is_success() {
        let msg = text.chars().take(200).collect::<String>();
        return Err(ApiError::new(
            if status.is_client_error() {
                status
            } else {
                StatusCode::BAD_GATEWAY
            },
            format!("the bundled homeserver answered {status}: {msg}"),
        ));
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text.trim().to_string())))
}

/// The bundled homeserver as the owner sees it (never the admin password).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BundledStatus {
    /// The node is set up with a bundled homeserver.
    pub configured: bool,
    /// Its admin API answered.
    pub reachable: bool,
    /// The homeserver's public key (z32): what accounts on it are "on".
    pub public_key: Option<String>,
    pub version: Option<String>,
    pub users: Option<u64>,
    pub disabled_users: Option<u64>,
    pub disk_used_mb: Option<f64>,
    pub unused_invites: Option<u64>,
    /// Where the homeserver says it can be reached: its Pubky TLS address and its ICANN domain.
    pub pubky_address: Option<String>,
    pub icann_domain: Option<String>,
    /// Why it is not reachable.
    pub error: Option<String>,
}

pub(crate) fn status_from_info(v: &Value) -> BundledStatus {
    let s = |k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(str::to_string);
    BundledStatus {
        configured: true,
        reachable: true,
        public_key: s("public_key"),
        version: s("version"),
        users: v["num_users"].as_u64(),
        disabled_users: v["num_disabled_users"].as_u64(),
        disk_used_mb: v["total_disk_used_mb"].as_f64(),
        unused_invites: v["num_unused_signup_codes"].as_u64(),
        pubky_address: s("pkarr_pubky_address"),
        icann_domain: s("pkarr_icann_domain"),
        error: None,
    }
}

/// `GET /v1/homeserver/bundled`: whether the node has a bundled homeserver and how it is doing.
pub(crate) async fn status(State(s): State<Arc<NodeState>>) -> ApiResult<BundledStatus> {
    if s.bundled_homeserver().is_none() {
        return Ok(Json(BundledStatus::default()));
    }
    Ok(Json(match admin(&s, reqwest::Method::GET, "/info", None).await {
        Ok(v) => status_from_info(&v),
        Err(e) => BundledStatus {
            configured: true,
            error: Some(e.message),
            ..Default::default()
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_needs_both_and_hides_the_password() {
        assert!(AdminEndpoint::new("http://homeserver:6288", "").is_none());
        assert!(AdminEndpoint::new("homeserver:6288", "pw").is_none());
        let ep = AdminEndpoint::new("http://homeserver:6288/", "S3CRET").unwrap();
        assert_eq!(ep.url, "http://homeserver:6288");
        assert!(!format!("{ep:?}").contains("S3CRET"));
        let st = status_from_info(
            &serde_json::json!({"num_users": 2, "num_disabled_users": 0, "total_disk_used_mb": 1.5,
            "num_signup_codes": 3, "num_unused_signup_codes": 1, "public_key": "g94j", "pkarr_pubky_address": "127.0.0.1:6287",
            "pkarr_icann_domain": "music.example.org", "version": "0.16.2"}),
        );
        assert_eq!(
            (st.users, st.unused_invites, st.version.as_deref()),
            (Some(2), Some(1), Some("0.16.2"))
        );
        assert_eq!(st.icann_domain.as_deref(), Some("music.example.org"));
    }
}
