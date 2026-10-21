//! The few node calls the shell makes itself; everything else is the web UI.

use crate::settings::Settings;
use serde::Deserialize;
use std::time::Duration;

fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(8))
        .user_agent(concat!("pubky-music-desktop/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn api(s: &Settings, path: &str) -> String {
    format!("{}/v1/{path}", s.base().trim_end_matches('/'))
}

/// What the node says about itself.
#[derive(Debug, Deserialize, Default)]
pub struct Info {
    #[serde(default)]
    pub name: String,
    /// Stable node id (nodes before LAN discovery have none).
    #[serde(default)]
    pub id: String,
}

/// Why a probe failed.
#[derive(Debug)]
pub enum ProbeError {
    /// Nothing answered at that address (maybe the node moved).
    Unreachable(String),
    /// The node answered and said no, or something else went wrong.
    Other(String),
}

impl From<ProbeError> for String {
    fn from(e: ProbeError) -> String {
        match e {
            ProbeError::Unreachable(m) | ProbeError::Other(m) => m,
        }
    }
}

/// Check the node answers and accepts the token; returns its name and id.
pub async fn probe(s: &Settings) -> Result<Info, ProbeError> {
    let resp = client()
        .map_err(|e| ProbeError::Other(e.to_string()))?
        .get(api(s, "info"))
        .bearer_auth(&s.token)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() || e.is_connect() {
                ProbeError::Unreachable(format!("Can't reach {}", s.node_url))
            } else {
                ProbeError::Unreachable(format!("Can't reach {}: {e}", s.node_url))
            }
        })?;
    match resp.status().as_u16() {
        200..=299 => Ok(resp.json::<Info>().await.unwrap_or_default()),
        401 | 403 => Err(ProbeError::Other("The node refused the token".into())),
        code => Err(ProbeError::Other(format!("The node answered {code}"))),
    }
}

/// Register this machine's player: one zone per DAC appears in the UI.
#[cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]
pub async fn add_endpoint(s: &Settings, address: &str) -> Result<(), String> {
    let resp = client()
        .map_err(|e| e.to_string())?
        .post(api(s, "outputs/endpoints"))
        .bearer_auth(&s.token)
        .json(&serde_json::json!({ "address": address }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.error_for_status().map(|_| ()).map_err(|e| e.to_string())
}

/// Forget an old player address, so the zone list does not fill with ghosts.
#[cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]
pub async fn remove_endpoint(s: &Settings, address: &str) -> Result<(), String> {
    let enc = address
        .replace('%', "%25")
        .replace('/', "%2F")
        .replace(':', "%3A");
    let resp = client()
        .map_err(|e| e.to_string())?
        .delete(api(s, &format!("outputs/endpoints/{enc}")))
        .bearer_auth(&s.token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.error_for_status().map(|_| ()).map_err(|e| e.to_string())
}
