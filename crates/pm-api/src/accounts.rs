//! Accounts on the node, per person (STEWARD-BUNDLED-HOMESERVER.md, step 3): the owner and
//! listeners. Everyone signs in with Pubky Ring (`/v1/auth/pubky/start|poll`):
//! - the **owner** (the node's own Pubky identity) gets the node's API token, as before; apps
//!   and integrations keep using that token;
//! - a **listener** (someone in People with music access, `people::may_use_music`) gets a
//!   personal token, kept only as its hash. It opens the shared library, search, sources and the
//!   zones the owner allows; settings, People, the homeserver, sources' sign-ins and the
//!   library's folders stay the owner's. Taking music access away (People) stops the token at
//!   once.
//!
//! Each request carries a [`Caller`] (an extension) for handlers that care who is asking.
//! Listeners' own playlists, favourites and history (on their own homeserver) come with the
//! node agent's per-person sync; until then a listener's writes to them are refused.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const TOKENS: &str = "accounts.tokens";
const ZONES: &str = "accounts.listener_zones";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Listener,
    /// A party guest (party.rs): adds songs to one party, nothing else.
    Guest,
}

/// Who made a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub role: Role,
    /// Their public key (z32), when known.
    pub key: Option<String>,
}

/// A listener's token, as the node keeps it: the hash only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonToken {
    pub hash: String,
    pub key: String,
    pub created_at: i64,
}

fn hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn load_tokens(s: &NodeState) -> Vec<PersonToken> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(TOKENS)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

fn store_tokens(s: &NodeState, t: &[PersonToken]) -> Result<(), ApiError> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(TOKENS, &serde_json::to_string(t).expect("serializes"))?;
    Ok(())
}

/// A new personal token for `key` (returned once; the node keeps its hash).
pub fn mint(s: &NodeState, key: &str) -> Result<String, ApiError> {
    let key = key.trim_start_matches("pubky").to_string();
    let token = format!("pml_{}", crate::tidal::random_urlsafe(32));
    let mut all = load_tokens(s);
    // A person signing in again on another device keeps their other sessions; at most 10
    let mine: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, t)| t.key == key)
        .map(|(i, _)| i)
        .collect();
    if mine.len() >= 10 {
        all.remove(mine[0]);
    }
    all.push(PersonToken {
        hash: hash(&token),
        key,
        created_at: now(),
    });
    store_tokens(s, &all)?;
    Ok(token)
}

/// Whose token this is, when it is a listener's that still has music access.
fn listener_of(s: &NodeState, token: &str) -> Option<String> {
    if !token.starts_with("pml_") {
        return None;
    }
    let h = hash(token);
    let key = load_tokens(s).into_iter().find(|t| t.hash == h)?.key;
    crate::people::may_use_music(s, &key).then_some(key)
}

/// The zones a listener may play on: the owner's list, or every zone when none is set.
pub fn listener_zones(s: &NodeState) -> Option<Vec<String>> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(ZONES)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
        .filter(|v| !v.is_empty())
}

/// Routes a listener may never use (reads included): the owner's settings and data.
const OWNER_ONLY: &[&str] = &[
    "/people",
    "/party",
    "/remote",
    "/homeserver",
    "/node",
    "/pubky",
    "/backup",
    "/storage",
    "/library/webdav",
    "/library/uploads",
    "/library/imports",
    "/library/folders",
    "/library/scan",
    "/library/tag-fixes",
    "/outputs",
];

/// What a listener may read: the shared library, public catalogues and directories, their
/// zones, and who they are. Everything else (the owner's favourites, playlists, tags, reviews,
/// history, podcast subscriptions and streaming accounts) stays the owner's.
const LISTENER_READS: &[&str] = &[
    "/me",
    "/events",
    "/info",
    "/status",
    "/health",
    "/library/albums",
    "/library/artists",
    "/library/tracks",
    "/library/genres",
    "/library/search",
    "/library/stats",
    "/lyrics",
    "/search",
    "/item",
    "/catalog/search",
    "/discover",
    "/shared",
    "/profiles",
    "/mb",
    "/radio/countries",
    "/radio/tags",
    "/radio/search",
    "/radio/curated",
    "/podcasts/search",
    "/zones",
];

/// Inside [`LISTENER_READS`], still the owner's: identification candidates and suggestions.
const LISTENER_READ_EXCEPTIONS: &[&str] = &["/candidates", "/suggestions"];

fn under(path: &str, prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// Whether a listener may use this zone: the owner's list, or every zone when none is set.
fn zone_refusal(s: &NodeState, zone: &str) -> Option<&'static str> {
    match listener_zones(s) {
        Some(allowed) if !allowed.iter().any(|z| z == zone) => {
            Some("the owner has not opened that zone to listeners")
        }
        _ => None,
    }
}

/// Whether a listener may make this request. `None` = yes; `Some(why)` = no.
pub fn listener_refusal(s: &NodeState, method: &Method, path: &str) -> Option<&'static str> {
    // Inside the `/v1` router the prefix is gone; take either
    let path = path.strip_prefix("/v1").unwrap_or(path);
    if under(path, OWNER_ONLY) {
        return Some("that is the owner's");
    }
    if *method == Method::GET || *method == Method::HEAD {
        if !under(path, LISTENER_READS) || LISTENER_READ_EXCEPTIONS.iter().any(|x| path.contains(x)) {
            return Some("that is the owner's");
        }
        // A zone's state and queue: only the zones open to listeners (the list itself is fine)
        if let Some(rest) = path.strip_prefix("/zones/") {
            return zone_refusal(s, rest.split('/').next().unwrap_or(rest));
        }
        return None;
    }
    // Playing on a zone the owner allows: its queue, transport, volume and the like
    if let Some(rest) = path.strip_prefix("/zones/") {
        let (zone, what) = rest.split_once('/').unwrap_or((rest, ""));
        if what.is_empty() || what.starts_with("dsp") {
            return Some("only the owner adds, removes or tunes zones");
        }
        return zone_refusal(s, zone);
    }
    // The page asks who is signed in, and a listener may sign out
    if path == "/me" || path.starts_with("/me/") {
        return None;
    }
    Some("listeners' own playlists, favourites and history are not built yet; this change would land in the owner's")
}

/// The request's caller, from its token: the API token is the owner; a listener's personal
/// token is a listener. `None` = not signed in.
pub fn caller(s: &NodeState, token: Option<&str>) -> Option<Caller> {
    let owner_key = || {
        s.sync_handle()
            .map(|h| h.z32().trim_start_matches("pubky").to_string())
    };
    let Some(api) = &s.token else {
        return Some(Caller {
            role: Role::Owner,
            key: owner_key(),
        });
    };
    let t = token?;
    if t == api.as_str() {
        return Some(Caller {
            role: Role::Owner,
            key: owner_key(),
        });
    }
    if t.starts_with(crate::party::GUEST_PREFIX) {
        return crate::party::guest_of(s, t).map(|_| Caller {
            role: Role::Guest,
            key: None,
        });
    }
    listener_of(s, t).map(|key| Caller {
        role: Role::Listener,
        key: Some(key),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub role: Role,
    pub key: Option<String>,
    /// For a listener: the zones they may play on (`null` = all).
    pub zones: Option<Vec<String>>,
}

/// `GET /v1/me`: who this token is.
pub(crate) async fn me(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
) -> ApiResult<Me> {
    Ok(Json(Me {
        zones: (c.role == Role::Listener).then(|| listener_zones(&s)).flatten(),
        role: c.role,
        key: c.key,
    }))
}

/// `DELETE /v1/me/token`: a listener signs this device out (the token stops working).
pub(crate) async fn sign_out(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
    headers: axum::http::HeaderMap,
) -> ApiResult<serde_json::Value> {
    if c.role != Role::Listener {
        return Err(ApiError::bad_request(
            "the owner's API token is set in the node's config, not signed out",
        ));
    }
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let h = hash(token);
    let mut all = load_tokens(&s);
    all.retain(|t| t.hash != h);
    store_tokens(&s, &all)?;
    Ok(Json(serde_json::json!({"signed_out": true})))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListenerZones {
    /// Zone ids listeners may play on; empty = every zone.
    pub zones: Vec<String>,
}

/// `GET /v1/people/zones`: the zones open to listeners (empty = all).
pub(crate) async fn get_zones(State(s): State<Arc<NodeState>>) -> ApiResult<ListenerZones> {
    Ok(Json(ListenerZones {
        zones: listener_zones(&s).unwrap_or_default(),
    }))
}

/// `PUT /v1/people/zones {zones}`.
pub(crate) async fn set_zones(
    State(s): State<Arc<NodeState>>,
    Json(b): Json<ListenerZones>,
) -> ApiResult<ListenerZones> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(ZONES, &serde_json::to_string(&b.zones).expect("serializes"))?;
    Ok(Json(b))
}

/// Signed-in devices per person, for People.
pub fn sessions_of(s: &NodeState, key: &str) -> usize {
    load_tokens(s).iter().filter(|t| t.key == key).count()
}

/// Forgets every token of `key` (People › remove).
pub fn sign_out_everywhere(s: &NodeState, key: &str) -> Result<(), ApiError> {
    let mut all = load_tokens(s);
    all.retain(|t| t.key != key);
    store_tokens(s, &all)
}

/// The refusal a listener gets.
pub fn refused(why: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, format!("Not for listeners: {why}."))
}
