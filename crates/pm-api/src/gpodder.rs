//! gPodder sync: podcast subscriptions and listening positions kept in step with a gPodder
//! account, so a phone app (AntennaPod, Kasts, gPodder) and the node share them.
//!
//! Two servers speak the same shapes:
//! - **gPodder API v2** (gpodder.net, opodsync, and others): `api/2/subscriptions/<user>/<device>.json`
//!   and `api/2/episodes/<user>.json`, with HTTP basic auth.
//! - **Nextcloud's gpoddersync app**: `index.php/apps/gpoddersync/subscriptions`,
//!   `…/subscription_change/create`, `…/episode_action` and `…/episode_action/create`, with a
//!   Nextcloud user and app password.
//!
//! Each sync:
//! 1. Takes the server's subscription changes since the last sync: adds the new feeds and drops
//!    the removed ones.
//! 2. Sends the node's own changes since then. The first sync sends every subscription.
//! 3. Sends the node's position changes as `play` actions, then takes the server's: a newer one
//!    moves the node's position, and 95% or more of the episode counts as played.
//!
//! It runs every 15 minutes, and on `POST /v1/podcasts/gpodder/sync`. The password is kept in
//! the node's database, like a WebDAV share's, and is never returned.
//!
//! Follows Music Assistant's `gpodder` provider (Apache-2.0; see NOTICE).

use crate::podcasts::{self, Feed, Progress};
use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

const ACCOUNT: &str = "podcasts.gpodder";
const PENDING: &str = "podcasts.gpodder.pending";
const SYNC_EVERY: Duration = Duration::from_secs(15 * 60);
/// Share of an episode after which it counts as played.
const PLAYED_PERCENT: u64 = 95;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// gPodder API v2: gpodder.net, opodsync and other servers.
    #[default]
    Gpodder,
    /// Nextcloud's gpoddersync app.
    Nextcloud,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Account {
    #[serde(default)]
    kind: Kind,
    server: String,
    username: String,
    password: String,
    device: String,
    /// The server's timestamps for the next "since".
    #[serde(default)]
    subs_since: i64,
    #[serde(default)]
    actions_since: i64,
    /// Unix seconds up to which the node's positions were sent.
    #[serde(default)]
    sent_until: i64,
    #[serde(default)]
    last_sync: i64,
    #[serde(default)]
    last_error: Option<String>,
}

/// Subscription changes made on the node since the last sync.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Pending {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

fn account(s: &NodeState) -> Option<Account> {
    let a: Account = podcasts::load(s, ACCOUNT);
    (!a.server.is_empty()).then_some(a)
}

/// Note a subscription made on the node, for the next sync.
pub(crate) fn note_subscribed(s: &NodeState, url: &str) {
    note(s, url, true);
}

/// Note a subscription dropped on the node, for the next sync.
pub(crate) fn note_unsubscribed(s: &NodeState, url: &str) {
    note(s, url, false);
}

fn note(s: &NodeState, url: &str, add: bool) {
    if account(s).is_none() {
        return;
    }
    let mut p: Pending = podcasts::load(s, PENDING);
    p.add.retain(|u| u != url);
    p.remove.retain(|u| u != url);
    if add {
        p.add.push(url.to_string());
    } else {
        p.remove.push(url.to_string());
    }
    if let Err(e) = podcasts::store(s, PENDING, &p) {
        tracing::debug!("gPodder pending change not saved: {}", e.message);
    }
}

fn base(a: &Account) -> &str {
    a.server.trim_end_matches('/')
}

/// A URL path segment: anything but letters, digits and `-._~` percent-encoded.
fn seg(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn subs_get(a: &Account) -> String {
    match a.kind {
        Kind::Gpodder => format!(
            "{}/api/2/subscriptions/{}/{}.json",
            base(a),
            seg(&a.username),
            seg(&a.device)
        ),
        Kind::Nextcloud => format!("{}/index.php/apps/gpoddersync/subscriptions", base(a)),
    }
}

fn subs_post(a: &Account) -> String {
    match a.kind {
        Kind::Gpodder => subs_get(a),
        Kind::Nextcloud => format!(
            "{}/index.php/apps/gpoddersync/subscription_change/create",
            base(a)
        ),
    }
}

fn actions_get(a: &Account) -> String {
    match a.kind {
        Kind::Gpodder => format!("{}/api/2/episodes/{}.json", base(a), seg(&a.username)),
        Kind::Nextcloud => format!("{}/index.php/apps/gpoddersync/episode_action", base(a)),
    }
}

fn actions_post(a: &Account) -> String {
    match a.kind {
        Kind::Gpodder => actions_get(a),
        Kind::Nextcloud => format!("{}/index.php/apps/gpoddersync/episode_action/create", base(a)),
    }
}

#[derive(Debug, Deserialize, Default)]
struct SubsChanges {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
    #[serde(default)]
    timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Action {
    pub podcast: String,
    pub episode: String,
    pub action: String,
    /// UTC, `YYYY-MM-DDTHH:MM:SS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct Actions {
    #[serde(default)]
    actions: Vec<Action>,
    #[serde(default)]
    timestamp: i64,
}

#[derive(Debug, Deserialize, Default)]
struct Uploaded {
    #[serde(default)]
    timestamp: i64,
}

const ISO: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

fn iso(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(ISO).ok())
        .unwrap_or_default()
}

/// `YYYY-MM-DDTHH:MM:SS`, with or without fractions and a zone (taken as UTC).
fn unix(ts: &str) -> Option<i64> {
    let head = ts.get(..19)?;
    time::PrimitiveDateTime::parse(head, ISO)
        .ok()
        .map(|t| t.assume_utc().unix_timestamp())
}

/// What one sync did.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SyncReport {
    /// Feeds subscribed from the server, and dropped because the server dropped them.
    pub added: usize,
    pub removed: usize,
    /// Subscription changes and positions sent.
    pub sent_subscriptions: usize,
    pub sent_positions: usize,
    /// Positions taken from the server.
    pub positions: usize,
    /// Feeds the server listed that could not be fetched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
}

fn sync_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn send<T: for<'de> Deserialize<'de> + Default>(
    a: &Account,
    req: reqwest::RequestBuilder,
) -> Result<T, String> {
    let resp = req
        .basic_auth(&a.username, Some(&a.password))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("gPodder server: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err("the gPodder server refused the user name or password".into());
    }
    if !status.is_success() {
        return Err(format!("the gPodder server answered {status}"));
    }
    let text = resp.text().await.map_err(|e| format!("gPodder server: {e}"))?;
    if text.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&text).map_err(|e| format!("gPodder server: unexpected answer ({e})"))
}

/// One sync with `a` (not saved here). Fails on any server error, leaving the node unchanged
/// where it got that far.
async fn sync_with(s: &NodeState, a: &mut Account) -> Result<SyncReport, String> {
    let http = s.http_client();
    let mut report = SyncReport::default();
    let first = a.subs_since == 0;
    if first && a.kind == Kind::Gpodder {
        let url = format!(
            "{}/api/2/devices/{}/{}.json",
            base(a),
            seg(&a.username),
            seg(&a.device)
        );
        let body = serde_json::json!({"caption": "Pubky Music", "type": "server"});
        send::<serde_json::Value>(a, http.post(url).json(&body)).await?;
    }

    // 1. The server's subscription changes.
    let changes: SubsChanges = send(
        a,
        http.get(subs_get(a))
            .query(&[("since", a.subs_since.to_string())]),
    )
    .await?;
    let removed: HashSet<&str> = changes.remove.iter().map(|u| u.trim()).collect();
    let mut feeds = podcasts::feeds(s);
    let before = feeds.len();
    feeds.retain(|f| !removed.contains(f.url.as_str()));
    report.removed = before - feeds.len();
    let known: HashSet<String> = feeds.iter().map(|f| f.url.clone()).collect();
    let mut fetched: Vec<Feed> = Vec::new();
    for url in changes
        .add
        .iter()
        .map(|u| u.trim())
        .filter(|u| !known.contains(*u))
    {
        if removed.contains(url) || fetched.iter().any(|f| f.url == url) {
            continue;
        }
        match podcasts::fetch_feed(s, url).await {
            Ok(f) => fetched.push(f),
            Err(e) => {
                tracing::info!(feed = %url, "gPodder: feed not fetched: {}", e.message);
                report.failed.push(url.to_string());
            }
        }
    }
    report.added = fetched.len();
    let server_added: HashSet<String> = changes.add.iter().map(|u| u.trim().to_string()).collect();
    // Keep what came from the server even if the node's list changed meanwhile.
    let mut now_feeds = podcasts::feeds(s);
    now_feeds.retain(|f| !removed.contains(f.url.as_str()));
    for f in fetched {
        if !now_feeds.iter().any(|x| x.id == f.id) {
            now_feeds.push(f);
        }
    }
    podcasts::store(s, podcasts::FEEDS, &now_feeds).map_err(|e| e.message)?;
    let mut since = changes.timestamp;

    // 2. The node's own changes (on the first sync, every subscription).
    let mut pending: Pending = podcasts::load(s, PENDING);
    if first {
        for f in &now_feeds {
            if !pending.add.contains(&f.url) {
                pending.add.push(f.url.clone());
            }
        }
    }
    pending.add.retain(|u| !server_added.contains(u));
    pending.remove.retain(|u| !removed.contains(u.as_str()));
    if !pending.add.is_empty() || !pending.remove.is_empty() {
        let body = serde_json::json!({"add": pending.add, "remove": pending.remove});
        let up: Uploaded = send(a, http.post(subs_post(a)).json(&body)).await?;
        report.sent_subscriptions = pending.add.len() + pending.remove.len();
        since = since.max(up.timestamp);
    }
    podcasts::store(s, PENDING, &Pending::default()).map_err(|e| e.message)?;
    a.subs_since = since;

    // 3. Positions: the node's changes out, then the server's in.
    let started_at = podcasts::now();
    let feeds = podcasts::feeds(s);
    let prog = podcasts::progress(s);
    let mut out = Vec::new();
    for f in &feeds {
        for e in &f.episodes {
            let Some(p) = prog.get(&e.id) else { continue };
            if p.updated <= a.sent_until || (p.position_ms == 0 && !p.played) {
                continue;
            }
            let total = e.duration_ms.map(|d| d / 1000);
            let position = if p.played {
                total.unwrap_or(p.position_ms / 1000)
            } else {
                p.position_ms / 1000
            };
            out.push(Action {
                podcast: f.url.clone(),
                episode: e.url.clone(),
                action: "play".into(),
                timestamp: Some(iso(p.updated)),
                started: Some(0),
                position: Some(position),
                total,
                device: (a.kind == Kind::Gpodder).then(|| a.device.clone()),
            });
        }
    }
    if !out.is_empty() {
        send::<Uploaded>(a, http.post(actions_post(a)).json(&out)).await?;
        report.sent_positions = out.len();
    }
    a.sent_until = started_at;

    let remote: Actions = send(
        a,
        http.get(actions_get(a))
            .query(&[("since", a.actions_since.to_string())]),
    )
    .await?;
    let by_url: std::collections::HashMap<&str, &podcasts::Episode> = feeds
        .iter()
        .flat_map(|f| f.episodes.iter().map(|e| (e.url.as_str(), e)))
        .collect();
    let mut prog = podcasts::progress(s);
    for act in &remote.actions {
        if !act.action.eq_ignore_ascii_case("play") {
            continue;
        }
        let (Some(e), Some(pos)) = (by_url.get(act.episode.trim()), act.position) else {
            continue;
        };
        let at = act.timestamp.as_deref().and_then(unix).unwrap_or(0);
        let p: &mut Progress = prog.entry(e.id.clone()).or_default();
        if at <= p.updated {
            continue;
        }
        let total = act.total.filter(|t| *t > 0).or(e.duration_ms.map(|d| d / 1000));
        p.position_ms = pos * 1000;
        p.played = total.is_some_and(|t| pos * 100 >= t * PLAYED_PERCENT);
        // Older than now, so the next sync doesn't send it back.
        p.updated = at.min(started_at);
        report.positions += 1;
    }
    if report.positions > 0 {
        podcasts::store(s, podcasts::PROGRESS, &prog).map_err(|e| e.message)?;
    }
    a.actions_since = a.actions_since.max(remote.timestamp);
    Ok(report)
}

/// Sync now, if an account is set. Saves the outcome on the account.
pub async fn sync(s: &NodeState) -> Option<Result<SyncReport, String>> {
    let _one = sync_lock().lock().await;
    let mut a = account(s)?;
    let r = sync_with(s, &mut a).await;
    a.last_sync = podcasts::now();
    a.last_error = r.as_ref().err().cloned();
    if let Err(e) = podcasts::store(s, ACCOUNT, &a) {
        tracing::warn!("gPodder account not saved: {}", e.message);
    }
    match &r {
        Ok(rep) => tracing::info!(?rep, "gPodder sync"),
        Err(e) => tracing::warn!("gPodder sync: {e}"),
    }
    Some(r)
}

/// Sync every 15 minutes while an account is set and podcasts are on.
pub fn spawn_background(s: &Arc<NodeState>) {
    let weak = Arc::downgrade(s);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SYNC_EVERY);
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { return };
            if s.source_enabled(podcasts::SOURCE_ID) {
                sync(&s).await;
            }
        }
    });
}

// ---- routes ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Unix seconds; 0 before the first sync.
    #[serde(default)]
    pub last_sync: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

fn status_of(a: Option<&Account>) -> Status {
    match a {
        None => Status {
            configured: false,
            kind: None,
            server: None,
            username: None,
            device: None,
            last_sync: 0,
            last_error: None,
        },
        Some(a) => Status {
            configured: true,
            kind: Some(a.kind),
            server: Some(a.server.clone()),
            username: Some(a.username.clone()),
            device: Some(a.device.clone()),
            last_sync: a.last_sync,
            last_error: a.last_error.clone(),
        },
    }
}

/// `GET /v1/podcasts/gpodder`: the account (never its password) and the last sync.
pub(crate) async fn get(State(s): State<Arc<NodeState>>) -> ApiResult<Status> {
    crate::streaming::ensure_on(&s, podcasts::SOURCE_ID)?;
    Ok(Json(status_of(account(&s).as_ref())))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetAccount {
    #[serde(default)]
    pub kind: Kind,
    /// `https://gpodder.net`, an opodsync address, or the Nextcloud address.
    pub server: String,
    pub username: String,
    pub password: String,
    /// The device name the subscriptions are kept under (gPodder API; default `pubky-music`).
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connected {
    pub status: Status,
    pub sync: SyncReport,
}

/// `PUT /v1/podcasts/gpodder {kind?, server, username, password, device?}`: sign in with a first
/// sync, and keep the account only if it works.
pub(crate) async fn put(State(s): State<Arc<NodeState>>, Json(b): Json<SetAccount>) -> ApiResult<Connected> {
    crate::streaming::ensure_on(&s, podcasts::SOURCE_ID)?;
    let server = b.server.trim().trim_end_matches('/').to_string();
    if !(server.starts_with("https://") || server.starts_with("http://")) {
        return Err(ApiError::bad_request(
            "the server address starts with https:// or http://",
        ));
    }
    let username = b.username.trim().to_string();
    if username.is_empty() || b.password.is_empty() {
        return Err(ApiError::bad_request("give a user name and a password"));
    }
    let device = b
        .device
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "pubky-music".into());
    if !device
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ApiError::bad_request(
            "a device name uses letters, digits, '-', '_' and '.'",
        ));
    }
    let _one = sync_lock().lock().await;
    let mut a = Account {
        kind: b.kind,
        server,
        username,
        password: b.password,
        device,
        ..Account::default()
    };
    let report = sync_with(&s, &mut a)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
    a.last_sync = podcasts::now();
    podcasts::store(&s, ACCOUNT, &a)?;
    Ok(Json(Connected {
        status: status_of(Some(&a)),
        sync: report,
    }))
}

/// `DELETE /v1/podcasts/gpodder`: forget the account. Subscriptions and positions stay.
pub(crate) async fn delete(State(s): State<Arc<NodeState>>) -> ApiResult<Status> {
    crate::streaming::ensure_on(&s, podcasts::SOURCE_ID)?;
    let _one = sync_lock().lock().await;
    let lib = s.library.lock().expect("library poisoned");
    lib.delete_setting(ACCOUNT)?;
    lib.delete_setting(PENDING)?;
    Ok(Json(status_of(None)))
}

/// `POST /v1/podcasts/gpodder/sync`: sync now.
pub(crate) async fn sync_now(State(s): State<Arc<NodeState>>) -> ApiResult<SyncReport> {
    crate::streaming::ensure_on(&s, podcasts::SOURCE_ID)?;
    match sync(&s).await {
        None => Err(ApiError::not_found(
            "no gPodder account; PUT /v1/podcasts/gpodder first",
        )),
        Some(Ok(r)) => Ok(Json(r)),
        Some(Err(e)) => Err(ApiError::new(StatusCode::BAD_GATEWAY, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(iso(1_260_608_400), "2009-12-12T09:00:00");
        assert_eq!(unix("2009-12-12T09:00:00"), Some(1_260_608_400));
        assert_eq!(unix("2009-12-12T09:00:00.123Z"), Some(1_260_608_400));
        assert_eq!(unix("nonsense"), None);
    }

    #[test]
    fn urls_per_server_kind() {
        let mut a = Account {
            server: "https://gpodder.example/".into(),
            username: "ann".into(),
            device: "pubky-music".into(),
            ..Account::default()
        };
        assert_eq!(
            subs_get(&a),
            "https://gpodder.example/api/2/subscriptions/ann/pubky-music.json"
        );
        assert_eq!(
            actions_post(&a),
            "https://gpodder.example/api/2/episodes/ann.json"
        );
        a.username = "ann@example.org".into();
        assert_eq!(
            actions_get(&a),
            "https://gpodder.example/api/2/episodes/ann%40example.org.json"
        );
        a.kind = Kind::Nextcloud;
        assert_eq!(
            subs_get(&a),
            "https://gpodder.example/index.php/apps/gpoddersync/subscriptions"
        );
        assert_eq!(
            subs_post(&a),
            "https://gpodder.example/index.php/apps/gpoddersync/subscription_change/create"
        );
        assert_eq!(
            actions_post(&a),
            "https://gpodder.example/index.php/apps/gpoddersync/episode_action/create"
        );
    }
}
