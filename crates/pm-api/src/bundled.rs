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
/// 404 without one, so Settings can hide "This homeserver" with one cheap call.
pub(crate) async fn status(State(s): State<Arc<NodeState>>) -> ApiResult<BundledStatus> {
    if s.bundled_homeserver().is_none() {
        return Err(ApiError::not_found("this node has no bundled homeserver"));
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

// ---- the owner's account ----

/// Library setting: the signup token minted for the owner, reused while it is unused.
const OWNER_TOKEN: &str = "homeserver.owner_signup_token";

/// `XXXX-XXXX-XXXX`, Crockford base32.
pub(crate) fn is_signup_token(t: &str) -> bool {
    t.len() == 14
        && t.split('-').count() == 3
        && t.split('-')
            .all(|p| p.len() == 4 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The owner's signup token: the one minted before if nobody used it, else a new one with
/// unlimited storage (the owner's library and files live here).
async fn owner_token(s: &NodeState) -> Result<String, ApiError> {
    let stored = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(OWNER_TOKEN)?;
    if let Some(t) = stored {
        let unused = admin(
            s,
            reqwest::Method::GET,
            "/signup_tokens?state=unused&limit=1000",
            None,
        )
        .await?;
        let still = unused["items"]
            .as_array()
            .is_some_and(|a| a.iter().any(|i| i["token"].as_str() == Some(t.as_str())));
        if still {
            return Ok(t);
        }
    }
    let v = admin(
        s,
        reqwest::Method::POST,
        "/generate_signup_token",
        Some(&serde_json::json!({"storage_quota_mb": "unlimited"})),
    )
    .await?;
    let t = v.as_str().unwrap_or_default().to_string();
    if !is_signup_token(&t) {
        return Err(unreachable("it did not hand out a signup token"));
    }
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(OWNER_TOKEN, &t)?;
    Ok(t)
}

/// `POST /v1/homeserver/bundled/owner-signup`: "Create your Pubky account on this node's
/// homeserver". The node mints the owner's signup token (unlimited storage) and starts a sign-up
/// for Pubky Ring: Ring creates the account on the bundled homeserver and approves the node's
/// access. Poll `GET /v1/pubky/login` as for a sign-in; the node is then signed in as the owner,
/// and playlists, favourites and files go to this homeserver.
pub(crate) async fn owner_signup(State(s): State<Arc<NodeState>>) -> ApiResult<crate::types::LoginStarted> {
    if let Some(h) = s.sync_handle() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "the node is already signed in as {}: sign out first (Settings › Pubky)",
                h.z32()
            ),
        ));
    }
    let info = admin(&s, reqwest::Method::GET, "/info", None).await?;
    let hs = info["public_key"]
        .as_str()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| unreachable("no public key in /info"))?
        .to_string();
    let token = owner_token(&s).await?;
    let flow = s
        .homeserver()
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?
        .start_signup("pubky-music-node", &hs, &token)
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let url = flow.url().to_string();
    *s.login.lock().await = Some(flow);
    tracing::info!(homeserver = %hs, "owner sign-up on the bundled homeserver started");
    Ok(Json(crate::types::LoginStarted {
        qr_svg: crate::qr_svg(&url),
        url,
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
        assert!(is_signup_token("AB12-CD34-EF56"));
        assert!(!is_signup_token("AB12CD34EF56") && !is_signup_token("AB12-CD34-EF5/"));
    }
}
