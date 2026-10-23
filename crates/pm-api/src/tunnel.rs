//! Remote access through a Cloudflare Tunnel for the bundled install (STEWARD-BUNDLED-HOMESERVER.md,
//! step 4). Settings › Remote access takes a tunnel token (Cloudflare Zero Trust → Networks →
//! Tunnels) and the two public hostnames, one for the homeserver and one for the music node.
//!
//! The node writes them into the folder it shares with cloudflared and the homeserver's config
//! wrapper (`PM_TUNNEL_DIR`; the compose bundle mounts one volume in all three), using the
//! wrapper's own contract: `token` (cloudflared reads it with `--token-file`) and `domain` (the
//! wrapper then publishes that hostname on 443 in the homeserver's PKARR record at its next
//! start). cloudflared picks the token up by itself; the homeserver needs one restart, which the
//! node cannot do (that would take the Docker socket, i.e. root on the host), so it shows the
//! command and then checks the result: the record, and both hostnames over HTTPS.
//!
//! The token is a secret: it is never returned, logged or sent anywhere.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MUSIC_HOST: &str = "tunnel.music_hostname";
pub const RESTART_COMMAND: &str = "docker compose up -d --force-recreate homeserver-config homeserver";

/// How to restart the homeserver on installs other than the compose bundle (`PM_TUNNEL_RESTART_HINT`,
/// plain words: the Umbrel app says to restart the app); then no command is shown.
fn restart_hint() -> Option<String> {
    std::env::var("PM_TUNNEL_RESTART_HINT")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

fn dir(s: &NodeState) -> Result<PathBuf, ApiError> {
    s.tunnel_dir().map(Path::to_path_buf).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "this node has no tunnel folder (the bundled install sets PM_TUNNEL_DIR)",
        )
    })
}

/// A Cloudflare tunnel token: base64 of JSON with the account tag `a`, tunnel id `t` and
/// secret `s` (what `cloudflared tunnel token` prints and the dashboard shows).
pub(crate) fn valid_token(t: &str) -> bool {
    let t = t.trim();
    if t.len() < 40 || t.len() > 4096 {
        return false;
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(t)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(t));
    raw.ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| {
            ["a", "t", "s"]
                .iter()
                .all(|k| v[*k].as_str().is_some_and(|x| !x.is_empty()))
        })
}

/// A public hostname: letters, digits, dots and dashes, with at least one dot.
pub(crate) fn valid_host(h: &str) -> Option<String> {
    let h = h
        .trim()
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    let ok = h.len() <= 253
        && h.contains('.')
        && !h.starts_with(['.', '-'])
        && !h.ends_with(['.', '-'])
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    ok.then_some(h)
}

fn read(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write(dir: &Path, name: &str, body: &str, mode: u32) -> Result<(), ApiError> {
    let tmp = dir.join(format!(".{name}.tmp"));
    let io =
        |e: std::io::Error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("tunnel folder: {e}"));
    std::fs::write(&tmp, format!("{body}\n")).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(io)?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, dir.join(name)).map_err(io)
}

/// Remote access as Settings shows it. Never the token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TunnelStatus {
    /// The install has a tunnel folder (the compose bundle).
    pub available: bool,
    /// A token is saved.
    pub configured: bool,
    pub homeserver_domain: Option<String>,
    pub music_hostname: Option<String>,
    /// The homeserver's record already points at `homeserver_domain` on 443.
    pub published: Option<bool>,
    /// The homeserver still has to be restarted to publish the domain: run `restart_command`.
    pub restart_needed: bool,
    pub restart_command: Option<String>,
    /// The same in plain words, on installs where a command doesn't fit (Umbrel).
    #[serde(default)]
    pub restart_hint: Option<String>,
    /// `https://<homeserver_domain>/` answers as a Pubky homeserver.
    pub homeserver_reachable: Option<bool>,
    /// `https://<music_hostname>/` reaches this node.
    pub music_reachable: Option<bool>,
}

async fn https_ok(s: &NodeState, url: &str, homeserver: bool) -> bool {
    let Ok(resp) = s
        .http_client()
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    else {
        return false;
    };
    let st = resp.status();
    let json = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("json"));
    if homeserver {
        st.is_success() && resp.text().await.is_ok_and(|t| t.contains("Pubky Homeserver"))
    } else {
        // The node answers its API with JSON: 200, or 401 without the token, both mean "reached"
        (st.is_success() || st == StatusCode::UNAUTHORIZED) && json
    }
}

/// `GET /v1/remote/tunnel` (`?check=1` also tries both hostnames over HTTPS).
pub(crate) async fn status(
    State(s): State<Arc<NodeState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult<TunnelStatus> {
    let Some(d) = s.tunnel_dir().map(Path::to_path_buf) else {
        return Ok(Json(TunnelStatus::default()));
    };
    let configured = read(&d, "token").is_some();
    let domain = read(&d, "domain");
    let music = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(MUSIC_HOST)?;
    let published = match (&domain, s.bundled_homeserver()) {
        (Some(dom), Some(_)) => crate::bundled::admin(&s, reqwest::Method::GET, "/info", None)
            .await
            .ok()
            .map(|v| v["pkarr_icann_domain"].as_str() == Some(format!("{dom}:443").as_str())),
        _ => None,
    };
    let check = q.get("check").is_some_and(|v| v == "1" || v == "true");
    let homeserver_reachable = match (&domain, check) {
        (Some(dom), true) => Some(https_ok(&s, &format!("https://{dom}/"), true).await),
        _ => None,
    };
    let music_reachable = match (&music, check) {
        (Some(h), true) => Some(https_ok(&s, &format!("https://{h}/v1/info"), false).await),
        _ => None,
    };
    let restart_needed = configured && published == Some(false);
    Ok(Json(TunnelStatus {
        available: true,
        configured,
        homeserver_domain: domain,
        music_hostname: music,
        published,
        restart_needed,
        restart_command: (restart_needed && restart_hint().is_none()).then(|| RESTART_COMMAND.to_string()),
        restart_hint: restart_needed.then(restart_hint).flatten(),
        homeserver_reachable,
        music_reachable,
    }))
}

#[derive(Clone, Deserialize)]
pub struct TunnelSetup {
    pub token: String,
    pub homeserver_domain: String,
    #[serde(default)]
    pub music_hostname: Option<String>,
}

/// `PUT /v1/remote/tunnel {token, homeserver_domain, music_hostname?}`. In the tunnel's public
/// hostnames (Cloudflare), send `homeserver_domain` to `http://localhost:6286` and
/// `music_hostname` to `http://localhost:7788`.
pub(crate) async fn setup(
    State(s): State<Arc<NodeState>>,
    Json(b): Json<TunnelSetup>,
) -> ApiResult<TunnelStatus> {
    let d = dir(&s)?;
    if !valid_token(&b.token) {
        return Err(ApiError::bad_request(
            "that is not a Cloudflare tunnel token (Zero Trust → Networks → Tunnels → your tunnel → the token in the install command)",
        ));
    }
    let domain = valid_host(&b.homeserver_domain)
        .ok_or_else(|| ApiError::bad_request("the homeserver's hostname is not a valid domain name"))?;
    let music = match b.music_hostname.as_deref().filter(|h| !h.trim().is_empty()) {
        Some(h) => Some(
            valid_host(h)
                .ok_or_else(|| ApiError::bad_request("the music hostname is not a valid domain name"))?,
        ),
        None => None,
    };
    if music.as_deref() == Some(domain.as_str()) {
        return Err(ApiError::bad_request(
            "the homeserver and the music node need two different hostnames",
        ));
    }
    // Readable by cloudflared (another user in its own container); the folder is only mounted
    // into the bundle's containers.
    write(&d, "token", b.token.trim(), 0o644)?;
    write(&d, "domain", &domain, 0o644)?;
    {
        let lib = s.library.lock().expect("library poisoned");
        match &music {
            Some(h) => lib.set_setting(MUSIC_HOST, h)?,
            None => {
                lib.delete_setting(MUSIC_HOST)?;
            }
        }
    }
    tracing::info!(%domain, music = ?music, "Cloudflare tunnel saved");
    status(State(s), axum::extract::Query(Default::default())).await
}

/// `DELETE /v1/remote/tunnel`: removes the token and domain (the homeserver goes back to its LAN
/// address at its next start; the tunnel and DNS records in Cloudflare stay, delete them there).
pub(crate) async fn remove(State(s): State<Arc<NodeState>>) -> ApiResult<TunnelStatus> {
    let d = dir(&s)?;
    for f in ["token", "domain"] {
        let _ = std::fs::remove_file(d.join(f));
    }
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(MUSIC_HOST)?;
    status(State(s), axum::extract::Query(Default::default())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_hostnames() {
        let tok = base64::engine::general_purpose::STANDARD.encode(
            r#"{"a":"0123456789abcdef","t":"6ff42ae2-765d-4adf-8112-31c55c1551ef","s":"c2VjcmV0c2VjcmV0"}"#,
        );
        assert!(valid_token(&tok));
        assert!(!valid_token("not-a-token"));
        assert!(!valid_token(
            &base64::engine::general_purpose::STANDARD.encode(r#"{"a":"x","t":"y"}"#)
        ));
        assert_eq!(
            valid_host("https://HS.Example.org/").as_deref(),
            Some("hs.example.org")
        );
        assert!(valid_host("localhost").is_none());
        assert!(valid_host("evil.org\nicann_domain=x").is_none());
        assert!(valid_host("-x.org").is_none());
    }
}
