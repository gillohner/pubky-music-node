//! TIDAL "Full tracks (unofficial)" (owner, 2026-10-22; STEWARD-TIDAL-FULL.md, the first
//! source of the dual-mode pattern in STEWARD-DUAL-MODE.md). The official app (Open API,
//! third-party tier) only gets 30 s previews. This opt-in mode signs in the way TIDAL's own TV
//! and desktop apps do (OAuth device flow: a code entered at link.tidal.com) and asks TIDAL's
//! private playback endpoint for the full stream. TIDAL has not approved it: it can stop working
//! at any time, and it needs a paid TIDAL plan.
//!
//! - **Client:** a TIDAL device client id and secret the owner enters in Settings (or
//!   `[tidal] unofficial_client_id` / `unofficial_client_secret`). The node never extracts or
//!   decodes credentials from another project's code, and never uses Music Assistant's bundled
//!   ones. Without a client the mode stays off.
//! - **Streams:** `playbackinfopostpaywall` answers with a BTS manifest (direct URLs) or a DASH
//!   manifest; both are read as one stream into the node's usual decoder. Anything encrypted is
//!   refused, never decrypted.
//! - **Health:** a failure of the mode itself (revoked client, changed answer) switches it off,
//!   playback falls back to the official preview, and the TIDAL page says why, with Retry.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const STATE_KEY: &str = "tidal.unofficial";
pub const CLIENT_KEY: &str = "tidal.unofficial_client";
const SCOPE: &str = "r_usr w_usr w_sub";
/// What the page says before the mode is switched on.
pub const WARNING: &str = "This uses TIDAL's private API, which TIDAL has not approved for other apps. It can stop working at any time, and TIDAL could act on the account. It needs a paid TIDAL plan.";

fn v1_base(s: &NodeState) -> String {
    s.tidal
        .read()
        .expect("tidal endpoints")
        .v1_base
        .trim_end_matches('/')
        .to_string()
}

fn auth_base(s: &NodeState) -> String {
    s.tidal
        .read()
        .expect("tidal endpoints")
        .auth_base
        .trim_end_matches('/')
        .to_string()
}

/// A urlencoded form body (reqwest is built without its form feature here).
fn form(pairs: &[(&str, &str)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

fn now() -> i64 {
    pm_library::db::now_unix()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Pending {
    pub device_code: String,
    pub user_code: String,
    pub uri: String,
    pub expires_at: i64,
    pub interval: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
    pub at: i64,
}

/// What the node keeps for this mode (the session is a sign-in: a backup carries it only under
/// the owner's passphrase).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeState {
    #[serde(default)]
    pub enabled: bool,
    /// `max` (up to 24-bit/192 kHz) or `high` (16-bit/44.1 kHz).
    #[serde(default)]
    pub quality: String,
    #[serde(default)]
    pub session: Option<Session>,
    #[serde(default)]
    pub login: Option<Pending>,
    #[serde(default)]
    pub health: Option<Health>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Client {
    pub id: String,
    pub secret: String,
    /// `settings` or `config`.
    pub from: String,
}

pub fn load(lib: &pm_library::Library) -> ModeState {
    lib.get_setting(STATE_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn save(lib: &pm_library::Library, st: &ModeState) {
    let _ = lib.set_setting(STATE_KEY, &serde_json::to_string(st).expect("state serializes"));
}

fn update(s: &NodeState, f: impl FnOnce(&mut ModeState)) -> ModeState {
    let lib = s.library.lock().expect("library poisoned");
    let mut st = load(&lib);
    f(&mut st);
    save(&lib, &st);
    st
}

fn config_client() -> &'static std::sync::OnceLock<Client> {
    static C: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    &C
}

/// The device client from node.toml (`[tidal] unofficial_client_id` / `unofficial_client_secret`),
/// set once at start-up; Settings override it.
pub fn set_config_client(id: &str, secret: &str) {
    if !id.trim().is_empty() {
        let _ = config_client().set(Client {
            id: id.trim().into(),
            secret: secret.trim().into(),
            from: "config".into(),
        });
    }
}

/// The device client the owner set: in Settings, else from node.toml, else the
/// `PM_TIDAL_UNOFFICIAL_CLIENT_ID` / `_SECRET` environment, else the one built into this build
/// (`from: builtin`, python-tidal's, STEWARD-TIDAL-DEFAULT.md).
pub fn client_of(lib: &pm_library::Library) -> Option<Client> {
    let kept: Option<Client> = lib
        .get_setting(CLIENT_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok());
    if let Some(c) = kept.filter(|c| !c.id.is_empty()) {
        return Some(c);
    }
    if let Some(c) = config_client().get() {
        return Some(c.clone());
    }
    let id = std::env::var("PM_TIDAL_UNOFFICIAL_CLIENT_ID").unwrap_or_default();
    let secret = std::env::var("PM_TIDAL_UNOFFICIAL_CLIENT_SECRET").unwrap_or_default();
    if !id.trim().is_empty() {
        return Some(Client {
            id: id.trim().into(),
            secret: secret.trim().into(),
            from: "config".into(),
        });
    }
    crate::project_apps::tidal_device_client().map(|(id, secret)| Client {
        id,
        secret,
        from: "builtin".into(),
    })
}

/// This build carries a device client of its own.
pub fn builtin_client() -> bool {
    crate::project_apps::tidal_device_client().is_some()
}

/// Whether the mode is on and signed in, so TIDAL tracks can play in full.
pub fn streams(s: &NodeState) -> bool {
    let st = load(&s.library.lock().expect("library poisoned"));
    st.enabled && st.session.is_some()
}

fn client(s: &NodeState) -> Result<Client, String> {
    client_of(&s.library.lock().expect("library poisoned"))
        .ok_or_else(|| "Set a TIDAL device client id and secret first (Sources › TIDAL › Full tracks)".into())
}

fn session_from(v: &Value, previous: Option<&Session>) -> Option<Session> {
    Some(Session {
        access_token: v["access_token"].as_str()?.to_string(),
        refresh_token: v["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at: now() + v["expires_in"].as_i64().unwrap_or(3600),
        user_id: previous.and_then(|p| p.user_id.clone()),
        country: previous.map(|p| p.country.clone()).unwrap_or_default(),
        session_id: previous.and_then(|p| p.session_id.clone()),
    })
}

/// Who the session is: TIDAL's `/sessions` (user, country, session id).
async fn fill_session(s: &NodeState, mut sess: Session) -> Session {
    if let Ok(r) = s
        .http_client()
        .get(format!("{}/sessions", v1_base(s)))
        .bearer_auth(&sess.access_token)
        .send()
        .await
    {
        if let Ok(v) = r.json::<Value>().await {
            sess.user_id = v["userId"].as_i64().map(|u| u.to_string()).or(sess.user_id);
            if let Some(c) = v["countryCode"].as_str() {
                sess.country = c.to_string();
            }
            sess.session_id = v["sessionId"].as_str().map(str::to_string).or(sess.session_id);
        }
    }
    sess
}

// ---- status and settings -----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnofficialStatus {
    pub service: String,
    pub enabled: bool,
    pub signed_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub quality: String,
    /// A sign-in waiting for the code to be entered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_from: Option<String>,
    /// This build has a built-in device client (python-tidal's); "Use built-in" goes back to it.
    pub builtin: bool,
    pub warning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingView {
    pub user_code: String,
    pub uri: String,
    pub expires_at: i64,
}

fn view(s: &NodeState) -> UnofficialStatus {
    let lib = s.library.lock().expect("library poisoned");
    let st = load(&lib);
    let client_from = client_of(&lib).map(|c| c.from);
    UnofficialStatus {
        service: "tidal".into(),
        enabled: st.enabled,
        signed_in: st.session.is_some(),
        user: st.session.as_ref().and_then(|x| x.user_id.clone()),
        quality: if st.quality.is_empty() {
            "max".into()
        } else {
            st.quality.clone()
        },
        pending: st
            .login
            .as_ref()
            .filter(|p| p.expires_at > now())
            .map(|p| PendingView {
                user_code: p.user_code.clone(),
                uri: p.uri.clone(),
                expires_at: p.expires_at,
            }),
        health: st.health.clone(),
        client_from,
        builtin: builtin_client(),
        warning: WARNING.into(),
    }
}

/// `GET /v1/sources/tidal/unofficial`.
pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<UnofficialStatus> {
    Ok(Json(view(&s)))
}

#[derive(Debug, Deserialize)]
pub struct UnofficialUpdate {
    pub enabled: Option<bool>,
    pub quality: Option<String>,
    /// Replace the device client (both; an empty id forgets it and goes back to node.toml's).
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// `PUT /v1/sources/tidal/unofficial {enabled?, quality?, client_id?, client_secret?}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<UnofficialUpdate>,
) -> ApiResult<UnofficialStatus> {
    if let Some(q) = &u.quality {
        if q != "max" && q != "high" {
            return Err(ApiError::bad_request("quality is max or high"));
        }
    }
    if let Some(id) = &u.client_id {
        let lib = s.library.lock().expect("library poisoned");
        if id.trim().is_empty() {
            lib.delete_setting(CLIENT_KEY)?;
        } else {
            let c = Client {
                id: id.trim().into(),
                secret: u.client_secret.clone().unwrap_or_default().trim().into(),
                from: "settings".into(),
            };
            lib.set_setting(CLIENT_KEY, &serde_json::to_string(&c).expect("client serializes"))?;
        }
    }
    update(&s, |st| {
        if let Some(e) = u.enabled {
            st.enabled = e;
            if e {
                // Switching it on again is the Retry
                st.health = None;
            }
        }
        if let Some(q) = u.quality {
            st.quality = q;
        }
    });
    Ok(Json(view(&s)))
}

/// `POST /v1/sources/tidal/unofficial/login`: the device code to enter at link.tidal.com; the
/// node waits for the approval in the background.
pub(crate) async fn login(State(s): State<Arc<NodeState>>) -> ApiResult<UnofficialStatus> {
    let c = client(&s).map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
    let resp = s
        .http_client()
        .post(format!("{}/v1/oauth2/device_authorization", auth_base(&s)))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[("client_id", c.id.as_str()), ("scope", SCOPE)]))
        .send()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("TIDAL: {e}")))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!(
                "TIDAL refused the sign-in ({status}): {}",
                v["error_description"]
                    .as_str()
                    .or(v["error"].as_str())
                    .unwrap_or("")
            ),
        ));
    }
    let uri = v["verificationUriComplete"]
        .as_str()
        .or(v["verificationUri"].as_str())
        .unwrap_or("link.tidal.com")
        .to_string();
    let pending = Pending {
        device_code: v["deviceCode"].as_str().unwrap_or_default().to_string(),
        user_code: v["userCode"].as_str().unwrap_or_default().to_string(),
        uri: if uri.starts_with("http") {
            uri
        } else {
            format!("https://{uri}")
        },
        expires_at: now() + v["expiresIn"].as_i64().unwrap_or(300),
        interval: v["interval"].as_u64().unwrap_or(2).clamp(1, 30),
    };
    if pending.device_code.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "TIDAL sent no device code",
        ));
    }
    update(&s, |st| st.login = Some(pending.clone()));
    let st2 = s.clone();
    tokio::spawn(async move { wait_for_approval(&st2, c, pending).await });
    Ok(Json(view(&s)))
}

async fn wait_for_approval(s: &Arc<NodeState>, c: Client, p: Pending) {
    while now() < p.expires_at {
        tokio::time::sleep(std::time::Duration::from_secs(p.interval)).await;
        // A newer sign-in replaced this one
        let current = load(&s.library.lock().expect("library poisoned")).login;
        if current.as_ref().map(|x| &x.device_code) != Some(&p.device_code) {
            return;
        }
        let Ok(resp) = s
            .http_client()
            .post(format!("{}/v1/oauth2/token", auth_base(s)))
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form(&[
                ("client_id", c.id.as_str()),
                ("client_secret", c.secret.as_str()),
                ("device_code", p.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("scope", SCOPE),
            ]))
            .send()
            .await
        else {
            continue;
        };
        let ok = resp.status().is_success();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if ok {
            if let Some(sess) = session_from(&v, None) {
                let sess = fill_session(s, sess).await;
                update(s, |st| {
                    st.session = Some(sess);
                    st.login = None;
                    st.enabled = true;
                    st.health = Some(Health {
                        ok: true,
                        detail: "signed in".into(),
                        at: now(),
                    });
                });
                tracing::info!("TIDAL full tracks (unofficial): signed in");
            }
            return;
        }
        if v["error"].as_str() == Some("expired_token") {
            break;
        }
    }
    update(s, |st| {
        if st.login.as_ref().map(|x| &x.device_code) == Some(&p.device_code) {
            st.login = None;
        }
    });
}

/// `DELETE /v1/sources/tidal/unofficial/login`: forget the session (the mode stays as set).
pub(crate) async fn logout(State(s): State<Arc<NodeState>>) -> ApiResult<UnofficialStatus> {
    update(&s, |st| {
        st.session = None;
        st.login = None;
    });
    Ok(Json(view(&s)))
}

// ---- playback (the audio thread and the browser proxy) -----------------------------------------

/// A full stream: the reader, the extension the decoder probes with, and the media type.
pub struct FullStream {
    pub reader: Box<dyn std::io::Read + Send>,
    pub ext: &'static str,
    pub mime: &'static str,
}

/// Whether a failure is the mode's (switch it off) or only this track's.
#[derive(Debug)]
pub enum FullError {
    Mode(String),
    Track(String),
}

fn token_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    auth: &str,
    client: &reqwest::blocking::Client,
) -> Result<Option<(Session, String)>, FullError> {
    let st = {
        let lib = library.lock().map_err(|_| FullError::Track("library".into()))?;
        load(&lib)
    };
    if !st.enabled {
        return Ok(None);
    }
    let Some(sess) = st.session.clone() else {
        return Ok(None);
    };
    let quality = if st.quality == "high" {
        "LOSSLESS"
    } else {
        "HI_RES_LOSSLESS"
    }
    .to_string();
    if sess.expires_at - now() > 60 {
        return Ok(Some((sess, quality)));
    }
    let c: Client = library
        .lock()
        .ok()
        .and_then(|l| client_of(&l))
        .ok_or_else(|| FullError::Mode("no TIDAL device client on this node".into()))?;
    let Some(refresh) = sess.refresh_token.clone() else {
        return Err(FullError::Mode("the session ran out: sign in again".into()));
    };
    let resp = client
        .post(format!("{auth}/v1/oauth2/token"))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", c.id.as_str()),
            ("client_secret", c.secret.as_str()),
            ("scope", SCOPE),
        ]))
        .send()
        .map_err(|e| FullError::Track(format!("renewing the session: {e}")))?;
    let status = resp.status();
    let v: Value = resp.json().unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(FullError::Mode(format!(
            "TIDAL did not renew the session ({status}): {}",
            v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or("")
        )));
    }
    let fresh =
        session_from(&v, Some(&sess)).ok_or_else(|| FullError::Mode("renewal without a token".into()))?;
    if let Ok(lib) = library.lock() {
        let mut st = load(&lib);
        st.session = Some(fresh.clone());
        save(&lib, &st);
    }
    Ok(Some((fresh, quality)))
}

/// Open the full track through the private playback endpoint. `Ok(None)`: the mode is off or
/// not signed in (use the official path).
pub fn open_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    ep: &crate::tidal::TidalEndpoints,
    id: &str,
) -> Result<Option<FullStream>, FullError> {
    let client = crate::sources::http_client();
    let auth = ep.auth_base.trim_end_matches('/');
    let v1 = ep.v1_base.trim_end_matches('/');
    let Some((sess, quality)) = token_blocking(library, auth, client)? else {
        return Ok(None);
    };
    let mut q: Vec<(&str, String)> = vec![
        ("playbackmode", "STREAM".into()),
        ("audioquality", quality),
        ("assetpresentation", "FULL".into()),
        ("countryCode", sess.country.clone()),
    ];
    if let Some(sid) = &sess.session_id {
        q.push(("sessionId", sid.clone()));
    }
    let resp = client
        .get(format!("{v1}/tracks/{id}/playbackinfopostpaywall"))
        .bearer_auth(&sess.access_token)
        .query(&q)
        .send()
        .map_err(|e| FullError::Track(format!("playback info: {e}")))?;
    let status = resp.status();
    let v: Value = resp.json().unwrap_or(Value::Null);
    match status.as_u16() {
        200..=299 => {}
        404 => return Err(FullError::Track("TIDAL has no stream for this track".into())),
        401 | 403 => {
            return Err(FullError::Mode(format!(
                "TIDAL refused the private playback ({status}): {}",
                v["userMessage"].as_str().or(v["error"].as_str()).unwrap_or("")
            )))
        }
        _ => return Err(FullError::Track(format!("playback info answered {status}"))),
    }
    if v["assetPresentation"].as_str().is_some_and(|p| p != "FULL") {
        return Err(FullError::Track(
            "TIDAL sent a preview: the plan may not include this track".into(),
        ));
    }
    let manifest = base64::engine::general_purpose::STANDARD
        .decode(v["manifest"].as_str().unwrap_or_default())
        .map_err(|_| FullError::Mode("playback info without a readable manifest".into()))?;
    let manifest = String::from_utf8_lossy(&manifest).into_owned();
    let kind = v["manifestMimeType"].as_str().unwrap_or_default();
    if kind == "application/vnd.tidal.bts" {
        let m: Value = serde_json::from_str(&manifest)
            .map_err(|_| FullError::Mode("BTS manifest is not JSON".into()))?;
        let enc = m["encryptionType"].as_str().unwrap_or("NONE");
        if enc != "NONE" {
            // Never decrypted: no DRM circumvention
            return Err(FullError::Track(format!(
                "TIDAL sends this track encrypted ({enc})"
            )));
        }
        let url = m["urls"][0]
            .as_str()
            .ok_or_else(|| FullError::Mode("BTS manifest without a URL".into()))?;
        let codecs = m["codecs"].as_str().unwrap_or_default().to_ascii_lowercase();
        let resp = client
            .get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| FullError::Track(format!("stream: {e}")))?;
        let (ext, mime) = if codecs.starts_with("flac") {
            ("flac", "audio/flac")
        } else {
            ("m4a", "audio/mp4")
        };
        return Ok(Some(FullStream {
            reader: Box::new(resp),
            ext,
            mime,
        }));
    }
    if kind == "application/dash+xml" {
        if manifest.contains("ContentProtection") {
            return Err(FullError::Track("TIDAL sends this track encrypted".into()));
        }
        let d = crate::dash::parse(&manifest).map_err(FullError::Mode)?;
        return Ok(Some(FullStream {
            reader: Box::new(crate::dash::DashReader::new(client, d.urls)),
            ext: "m4a",
            mime: "audio/mp4",
        }));
    }
    Err(FullError::Mode(format!("unknown manifest type {kind:?}")))
}

/// Record how the mode did: a failure of the mode itself switches it off (the page says why;
/// Retry switches it on again).
pub fn record(library: &std::sync::Mutex<pm_library::Library>, r: &Result<Option<FullStream>, FullError>) {
    let Ok(lib) = library.lock() else { return };
    let mut st = load(&lib);
    match r {
        Ok(Some(_)) => {
            st.health = Some(Health {
                ok: true,
                detail: "full tracks are playing".into(),
                at: now(),
            });
        }
        Err(FullError::Mode(e)) => {
            tracing::warn!("TIDAL full tracks (unofficial) switched off: {e}");
            st.enabled = false;
            st.health = Some(Health {
                ok: false,
                detail: e.clone(),
                at: now(),
            });
        }
        _ => return,
    }
    save(&lib, &st);
}
