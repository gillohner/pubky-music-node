//! Accounts on the node, per person (STEWARD-BUNDLED-HOMESERVER.md, step 3): the owner and
//! listeners. Everyone signs in with Pubky Ring (`/v1/auth/pubky/start|poll`):
//! - the **owner** (the node's own Pubky identity) gets a browser session of their own (`pmo_…`,
//!   kept only as its hash, ended by Log out); apps and integrations keep using the node's API
//!   token, which a browser may still be given by hand;
//! - a **listener** (someone in People with music access, `people::may_use_music`) gets a
//!   personal token, kept only as its hash. It opens the shared library, search, sources and the
//!   zones the owner allows; settings, People, the homeserver, sources' sign-ins and the
//!   library's folders stay the owner's. Taking music access away (People) stops the token at
//!   once.
//!
//! Each request carries a [`Caller`] (an extension) for handlers that care who is asking.
//! Listeners' own playlists, bookmarks and history (on their own homeserver) come with the
//! node agent's per-person sync; until then a listener's writes to them are refused.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::Json;
use pm_zones::FetchLimit;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const TOKENS: &str = "accounts.tokens";
const OWNER_SESSIONS: &str = "accounts.owner_sessions";
/// The start of an owner's browser session token.
pub const OWNER_PREFIX: &str = "pmo_";
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

fn load(s: &NodeState, setting: &str) -> Vec<PersonToken> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(setting)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

/// Changes a token list in place: read and saved under one hold of the library, so two changes
/// at once (a log in while Make a new token ends every session) can't bring ended ones back.
fn update<R>(
    s: &NodeState,
    setting: &str,
    f: impl FnOnce(&mut Vec<PersonToken>) -> R,
) -> Result<R, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let mut all: Vec<PersonToken> = lib
        .get_setting(setting)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let r = f(&mut all);
    lib.set_setting(setting, &serde_json::to_string(&all).expect("serializes"))?;
    Ok(r)
}

fn load_tokens(s: &NodeState) -> Vec<PersonToken> {
    load(s, TOKENS)
}

/// A new personal token for `key` (returned once; the node keeps its hash).
pub fn mint(s: &NodeState, key: &str) -> Result<String, ApiError> {
    let key = key.trim_start_matches("pubky").to_string();
    let token = format!("pml_{}", crate::tidal::random_urlsafe(32));
    update(s, TOKENS, |all| {
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
    })?;
    Ok(token)
}

/// A new browser session for the owner (key `key`), from a Pubky Ring sign-in: returned once,
/// the node keeps its hash. At most 20; the oldest goes first.
pub fn mint_owner(s: &NodeState, key: &str) -> Result<String, ApiError> {
    let token = format!("{OWNER_PREFIX}{}", crate::tidal::random_urlsafe(32));
    let key = key.trim_start_matches("pubky").to_string();
    let made = update(s, OWNER_SESSIONS, |all| {
        // Still the node's identity, checked under the same hold as the cleanup when it takes on
        // another one (`keep_owner_sessions_of`), so no session of the old one slips past it
        let current = s
            .sync_handle()
            .is_some_and(|h| h.z32().trim_start_matches("pubky") == key);
        if current {
            if all.len() >= 20 {
                all.remove(0);
            }
            all.push(PersonToken {
                hash: hash(&token),
                key: key.clone(),
                created_at: now(),
            });
        }
        current
    })?;
    if !made {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the node is signed in as someone else now: log in again",
        ));
    }
    Ok(token)
}

/// The answer to an owner's browser session while the node has no Pubky identity loaded (503, not
/// the 401 that makes the page log in again: the session works again once the identity is).
pub const IDENTITY_NOT_LOADED: &str = "this node couldn't load its Pubky identity (its homeserver didn't answer when it started), and browser sessions wait for it: restart the node once the homeserver answers. Until then the node's API token logs in";

/// An owner's browser session that is still open, for `owner`: the identity the node has loaded
/// now (a node signed in as someone else no longer lets the old owner's browsers in).
fn owner_session(s: &NodeState, token: &str, owner: &str) -> bool {
    let h = hash(token);
    load(s, OWNER_SESSIONS)
        .iter()
        .any(|t| t.hash == h && t.key == owner)
}

/// The node took on the identity `owner` (restored at start, or a log in renewed or changed it):
/// browser sessions of any other identity end, so none can come back later.
pub fn keep_owner_sessions_of(s: &NodeState, owner: &str) -> Result<(), ApiError> {
    let owner = owner.trim_start_matches("pubky");
    update(s, OWNER_SESSIONS, |all| all.retain(|t| t.key == owner))
}

/// Ends every browser session of the owner (a new API token: the answer to a leak).
pub fn end_owner_sessions(s: &NodeState) -> Result<(), ApiError> {
    update(s, OWNER_SESSIONS, Vec::clear)
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
    // The owner's listen history (under /me, which a listener may otherwise use)
    "/me/listens",
    "/people",
    "/vault",
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
/// zones, and who they are. Everything else (the owner's bookmarks, playlists, tags, reviews,
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
    "/devices",
    "/presence",
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
        // A listen-along group is open when every zone it plays for is (presence.rs)
        Some(allowed)
            if !allowed.iter().any(|z| z == zone)
                && !s
                    .presence
                    .sources(zone)
                    .is_some_and(|zs| zs.iter().all(|z| allowed.contains(z))) =>
        {
            Some("the owner has not opened that zone to listeners")
        }
        _ => None,
    }
}

/// Whether this caller may see and play on `zone`: the owner every zone; a listener the zones
/// the owner opened to listeners (every zone when none are chosen); a party guest none.
pub fn may_use_zone(s: &NodeState, c: &Caller, zone: &str) -> bool {
    match c.role {
        Role::Owner => true,
        Role::Listener => zone_refusal(s, zone).is_none(),
        Role::Guest => false,
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
        if what.is_empty() || what.starts_with("dsp") || what == "crossfade" {
            return Some("only the owner adds, removes or tunes zones");
        }
        return zone_refusal(s, zone);
    }
    // The page asks who is signed in, and a listener may sign out
    if path == "/me" || path.starts_with("/me/") {
        return None;
    }
    // Their own phones and laptops and what those keep offline (devices.rs checks the owner)
    if path.starts_with("/devices/") {
        return None;
    }
    // Listening along (presence.rs checks the zones)
    if path == "/presence/along" {
        return None;
    }
    Some("listeners' own playlists, bookmarks and history are not built yet; this change would land in the owner's")
}

/// The request's caller, from its token: the API token or an owner's browser session is the
/// owner; a listener's personal token is a listener. `None` = not signed in.
pub fn caller(s: &NodeState, token: Option<&str>) -> Option<Caller> {
    let owner_key = || {
        s.sync_handle()
            .map(|h| h.z32().trim_start_matches("pubky").to_string())
    };
    let Some(api) = s.token() else {
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
    if t.starts_with(OWNER_PREFIX) {
        // Only for the identity the node has loaded. Without one (its homeserver didn't answer at
        // start) no session is the owner: which identity is the owner then can't be told, and an
        // earlier owner's session must not come back. The API token still logs in meanwhile
        let key = owner_key()?;
        return owner_session(s, t, &key).then_some(Caller {
            role: Role::Owner,
            key: Some(key),
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

/// `DELETE /v1/me/token`: Log out of this browser. A listener's token or an owner's browser
/// session stops working; the node's API token (given to this browser by hand) stays as it is,
/// for the apps that use it: the page forgets it. `ended` says whether a session was ended.
pub(crate) async fn sign_out(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult<serde_json::Value> {
    // The token the request was let in with, from the header or `?token=` (as the middleware read it)
    let token = crate::bearer(&headers, &query).unwrap_or_default();
    let setting = match c.role {
        Role::Listener => TOKENS,
        Role::Owner if token.starts_with(OWNER_PREFIX) => OWNER_SESSIONS,
        _ => return Ok(Json(serde_json::json!({"signed_out": true, "ended": false}))),
    };
    let h = hash(&token);
    update(&s, setting, |all| all.retain(|t| t.hash != h))?;
    Ok(Json(serde_json::json!({"signed_out": true, "ended": true})))
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
    update(s, TOKENS, |all| all.retain(|t| t.key != key))
}

/// The refusal a listener gets.
pub fn refused(why: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, format!("Not for listeners: {why}."))
}

/// What a listener may put in a queue (or announce): library songs, the services' own track ids,
/// and http(s) links to public addresses, fetched only there (each redirect, playlist entry and
/// address it resolves to is checked when the node connects: the item carries
/// [`FetchLimit::PUBLIC`], whatever the request said). Never a path or a `file:` URL, nothing on
/// the node's machine or its network: those are the owner's to play. The URI is trimmed: the one
/// checked is the one queued.
pub fn check_listener_item(it: &mut pm_zones::QueueItem) -> Result<(), ApiError> {
    it.uri = it.uri.trim().to_string();
    it.limit = Some(it.limit.map_or(FetchLimit::PUBLIC, |l| l.and(FetchLimit::PUBLIC)));
    let uri = it.uri.clone();
    let uri = uri.as_str();
    use crate::*;
    const SERVICES: [&str; 13] = [
        sources::TRACK_SCHEME,
        tidal::TRACK_URI_PREFIX,
        deezer::TRACK_URI_PREFIX,
        qobuz::TRACK_URI_PREFIX,
        bandcamp::TRACK_URI_PREFIX,
        soundcloud::TRACK_URI_PREFIX,
        jamendo::TRACK_URI_PREFIX,
        opensubsonic::TRACK_URI_PREFIX,
        jellyfin::TRACK_URI_PREFIX,
        plex::TRACK_URI_PREFIX,
        audiobookshelf::TRACK_URI_PREFIX,
        youtube::VIDEO_URI_PREFIX,
        librespot::LIVE_URI,
    ];
    if SERVICES.iter().any(|p| uri.starts_with(p)) {
        return Ok(());
    }
    let public = FetchLimit::PUBLIC;
    let url = url::Url::parse(uri)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some());
    let Some(url) = url else {
        return Err(refused(
            "a listener queues songs from the library, the services' tracks and web links, not files or other addresses",
        ));
    };
    let literal = match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(std::net::IpAddr::from(ip)),
        Some(url::Host::Ipv6(ip)) => Some(std::net::IpAddr::from(ip)),
        _ => None,
    };
    if literal.is_some_and(|ip| !sources::allows(public, ip)) || url.host_str() == Some("localhost") {
        return Err(refused("that address is on the node's own network"));
    }
    // The URL as parsed is what is queued: `HTTP://…` or `http:\\…` would not be taken for a web
    // link when it plays (review of #177)
    it.uri = url.to_string();
    Ok(())
}

/// A listener's JSON answers show a library file by its name, not by where the owner keeps it
/// (`file.path` of every track, in lists, albums, search and item pages alike).
pub async fn listener_response(resp: axum::response::Response, roots: &[String]) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    // Only application/json answers are rewritten: streams (search's NDJSON, events, audio, covers)
    // pass as they are, so a stream that ever carries file paths needs its own filtering.
    let json = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !json {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "reading the answer failed").into_response();
    };
    let rewritten = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|mut v| {
            (hide_file_paths(&mut v) | hide_paths_under(&mut v, roots))
                .then(|| serde_json::to_vec(&v).ok())
                .flatten()
        });
    let body = match rewritten {
        Some(b) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            axum::body::Body::from(b)
        }
        None => bytes.into(),
    };
    axum::response::Response::from_parts(parts, body)
}

/// Replaces the folder part of every library file's path with nothing, leaving the file name:
/// in objects shaped like `pm_model::AudioFile` (`path`, `container`, `size_bytes`), in the
/// song page's "File" fact (`{label: "File", value}`, item.rs) and in queue items whose `uri` is
/// a file on the node (`/…`, `C:\\…`, `file:…`); then wherever else those paths appear in the
/// answer (a zone's error names the file it could not play). Whether anything changed.
pub fn hide_file_paths(v: &mut serde_json::Value) -> bool {
    let mut seen = Vec::new();
    let mut changed = hide_in(v, &mut seen);
    if !seen.is_empty() {
        seen.sort_by_key(|p: &String| std::cmp::Reverse(p.len()));
        seen.dedup();
        changed |= replace_paths(v, &seen);
    }
    changed
}

/// The folders the owner keeps music in (library folders and the homeserver's files), for
/// [`hide_paths_under`].
pub fn library_roots(s: &NodeState) -> Vec<String> {
    let mut roots: Vec<String> = s
        .library
        .lock()
        .expect("library poisoned")
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    if let Some(hs) = s.homeserver_files_dir() {
        roots.push(hs.to_string_lossy().into_owned());
    }
    roots.retain(|r| r.len() > 1);
    roots
}

/// Every path under one of `roots` in the answer's text, by its file name: a library song's
/// open error names its file (`cannot open /srv/music/A/x.flac: …`) where no queue item carries
/// it (review of #177). A path runs to `": "`, a quote or the end of the text.
pub fn hide_paths_under(v: &mut serde_json::Value, roots: &[String]) -> bool {
    use serde_json::Value;
    match v {
        Value::String(t) => {
            let mut changed = false;
            for root in roots {
                let root = root.trim_end_matches(['/', '\\']);
                let mut from = 0;
                while let Some(found) = t[from..].find(root) {
                    let at = from + found;
                    // A path on its own: at the start or after a space, quote, bracket, `=` or
                    // `:`, and the folder itself followed by a separator (never inside a URL)
                    let before_ok = t[..at]
                        .chars()
                        .next_back()
                        .is_none_or(|c| c.is_whitespace() || "\"'(=:".contains(c));
                    let after_ok = t[at + root.len()..].starts_with(['/', '\\']);
                    if !(before_ok && after_ok) {
                        from = at + root.len();
                        continue;
                    }
                    let rest = &t[at..];
                    let end = [": ", "\"", "\n"]
                        .iter()
                        .filter_map(|e| rest.find(e))
                        .min()
                        .unwrap_or(rest.len());
                    let name = file_name(rest[..end].trim_end_matches(['/', '\\']));
                    t.replace_range(at..at + end, &name);
                    from = at + name.len();
                    changed = true;
                }
            }
            changed
        }
        Value::Object(o) => o.values_mut().fold(false, |c, x| hide_paths_under(x, roots) | c),
        Value::Array(a) => a.iter_mut().fold(false, |c, x| hide_paths_under(x, roots) | c),
        _ => false,
    }
}

fn file_name(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or_default().to_string()
}

/// A queue item's `uri` that is a file on the node, not a link or a service's id.
fn local_path(u: &str) -> bool {
    let b = u.as_bytes();
    u.starts_with('/')
        || u.starts_with("\\\\")
        || u.starts_with("file:")
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
}

fn hide_in(v: &mut serde_json::Value, seen: &mut Vec<String>) -> bool {
    use serde_json::Value;
    match v {
        Value::Object(o) => {
            let mut changed = false;
            let field = if o.contains_key("container") && o.contains_key("size_bytes") {
                Some("path")
            } else if o.get("label").and_then(Value::as_str) == Some("File") {
                Some("value")
            } else if o.get("uri").and_then(Value::as_str).is_some_and(local_path) {
                Some("uri")
            } else {
                None
            };
            if let Some(Value::String(p)) = field.and_then(|f| o.get_mut(f)) {
                let name = file_name(p);
                if name != *p {
                    seen.push(std::mem::replace(p, name));
                    changed = true;
                }
            }
            for x in o.values_mut() {
                changed |= hide_in(x, seen);
            }
            changed
        }
        Value::Array(a) => a.iter_mut().fold(false, |c, x| hide_in(x, seen) | c),
        _ => false,
    }
}

/// Every hidden path, wherever else it is in the answer's text, by its file name.
fn replace_paths(v: &mut serde_json::Value, paths: &[String]) -> bool {
    use serde_json::Value;
    match v {
        Value::String(t) => {
            let mut changed = false;
            for p in paths {
                if t.contains(p.as_str()) {
                    *t = t.replace(p.as_str(), &file_name(p));
                    changed = true;
                }
            }
            changed
        }
        Value::Object(o) => o.values_mut().fold(false, |c, x| replace_paths(x, paths) | c),
        Value::Array(a) => a.iter_mut().fold(false, |c, x| replace_paths(x, paths) | c),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library song's open error names its file only in its text: under the library's folders
    /// it is cut to the file name, links are left alone (review of #177).
    #[test]
    fn paths_under_the_library_folders_become_file_names() {
        let roots = vec!["/srv/music".to_string(), "C:\\Music\\".to_string()];
        let mut v = serde_json::json!({
            "error": "cannot open /srv/music/My Album/01 x.flac: no decoder",
            "reason": "C:\\Music\\A\\y.flac",
            "links": ["https://musicbrainz.org/srv/music/z", "http://nas/srv/music/a.mp3", "/srv/musical/b.flac"],
        });
        assert!(hide_paths_under(&mut v, &roots));
        assert_eq!(v["error"], "cannot open 01 x.flac: no decoder");
        assert_eq!(v["reason"], "y.flac");
        assert_eq!(
            v["links"],
            serde_json::json!([
                "https://musicbrainz.org/srv/music/z",
                "http://nas/srv/music/a.mp3",
                "/srv/musical/b.flac"
            ])
        );
    }

    #[test]
    fn listeners_see_file_names_only() {
        let mut v = serde_json::json!({"tracks": [
            {"title": "A", "file": {"path": "/srv/music/Owner/A.flac", "container": "flac", "size_bytes": 1}},
            {"title": "B", "file": {"path": "webdav://nas/share/B.mp3", "container": "mp3", "size_bytes": 2}},
            {"title": "C", "file": {"path": "C:\\Music\\C.flac", "container": "flac", "size_bytes": 3}},
            {"title": "D", "file": {"path": "D.flac", "container": "flac", "size_bytes": 4}},
        ], "folder": {"path": "/srv/music"},
           "about": [{"label": "Format", "value": "FLAC 16/44.1"}, {"label": "File", "value": "/srv/music/Owner/A.flac"}]});
        assert!(hide_file_paths(&mut v));
        let names: Vec<_> = v["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["file"]["path"].clone())
            .collect();
        assert_eq!(names, ["A.flac", "B.mp3", "C.flac", "D.flac"]);
        // The song page's File fact too; other facts stay
        assert_eq!(v["about"][1]["value"], "A.flac");
        assert_eq!(v["about"][0]["value"], "FLAC 16/44.1");
        // Only library files: another object's `path` stays
        assert_eq!(v["folder"]["path"], "/srv/music");
        let mut plain = serde_json::json!({"file": {"path": "x.flac", "container": "flac", "size_bytes": 1}});
        assert!(!hide_file_paths(&mut plain));
        // A zone: queue items that are files, and its error naming one (security review
        // 2026-10-30, low)
        let mut zone = serde_json::json!({"id": "z", "error": "cannot open /srv/music/My Album/x.flac: broken",
            "queue": [{"id": 1, "uri": "/srv/music/My Album/x.flac"}, {"id": 2, "uri": "C:\\Music\\y.flac"},
                      {"id": 3, "uri": "track:4"}, {"id": 4, "uri": "https://radio.example.org/s.mp3"}]});
        assert!(hide_file_paths(&mut zone));
        assert_eq!(zone["error"], "cannot open x.flac: broken");
        let uris: Vec<_> = zone["queue"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| q["uri"].clone())
            .collect();
        assert_eq!(
            uris,
            ["x.flac", "y.flac", "track:4", "https://radio.example.org/s.mp3"]
        );
    }
}
