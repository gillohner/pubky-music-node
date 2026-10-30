//! "Share what I'm listening to" (listening together, L2; owner decision 2026-10-29): the owner's
//! listening as one public music `listening` post on their homeserver
//! (`/pub/music/v1/posts/{id}/{editId}.json`). Off by default.
//!
//! - **One post per listening session.** A song change, a pause, a resume or a seek is a new
//!   version of the same post (`started_at` when this song started, `paused_at` while paused); a
//!   stop is a version with `ended_at`, and the next play starts a new post. Never a post per song.
//! - **The node reconciles, it doesn't replay events:** every zone event of the owner's zone asks
//!   for one write of what the zone does now; writes take turns, so a burst of events is a write
//!   or two, and a version that says the same as the last one is not written.
//! - **A stopped node leaves nothing showing for long:** `expires_at` is a quarter of an hour past
//!   the song's end (an hour ahead when its length is unknown), renewed with each version.
//! - **Only the owner's plays** (not a listener's, not a party's: `listens::plays_are_the_owners`),
//!   and only what has a canonical link (a library file with no identity or a radio stream is not
//!   shared). A refused write (a full homeserver) turns sharing off, once, instead of retrying.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use pubky_music_specs::{MusicListeningContent, MusicPost, Root, LIVE_WINDOW_MAX_MICROS, MUSIC_NAMESPACE};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// `{share, name?}`: whether the owner shares what they listen to, and the session's name.
const SETTING: &str = "listening.share";
/// The post being written, while a session runs.
const POST: &str = "listening.post";
const MINUTE: i64 = 60_000_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ShareSetting {
    pub share: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The session post as last written.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Written {
    pub id: String,
    pub edit: String,
    pub zone: String,
    pub song: String,
    /// The first version's time: a session lasts a day at most (the spec's window).
    pub session_at: i64,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<i64>,
    pub expires_at: i64,
}

fn setting(s: &NodeState) -> ShareSetting {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

fn written(s: &NodeState) -> Option<Written> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(POST)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

fn store_written(s: &NodeState, w: Option<&Written>) {
    let lib = s.library.lock().expect("library poisoned");
    let r = match w {
        Some(w) => lib.set_setting(POST, &serde_json::to_string(w).expect("serializes")),
        None => lib.delete_setting(POST).map(|_| ()),
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, "storing the listening post");
    }
}

fn raw_key(z32: &str) -> &str {
    z32.strip_prefix("pubky").filter(|k| k.len() == 52).unwrap_or(z32)
}

/// One writer: reconciles take turns.
fn writer() -> &'static tokio::sync::Mutex<()> {
    static W: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

/// The canonical link of what a zone plays: the library song's, or the music link itself.
fn canonical(s: &NodeState, uri: &str) -> Option<String> {
    match uri
        .strip_prefix(crate::sources::TRACK_SCHEME)
        .and_then(|r| r.parse::<i64>().ok())
    {
        Some(id) => {
            let lib = s.library.lock().expect("library poisoned");
            let t = lib.get_track(pm_model::TrackId(id)).ok().flatten()?;
            t.publishable_uri().map(|u| u.to_url())
        }
        None => pm_model::MusicUri::parse(uri).ok().map(|u| u.to_url()),
    }
}

/// A zone event: the owner's zone, with sharing on, asks for one reconcile.
pub(crate) fn zone_event(s: &Arc<NodeState>, zone: &str, event: &pm_zones::ZoneEvent) {
    use pm_zones::ZoneEvent as E;
    let relevant = matches!(
        event,
        E::TrackStarted { .. } | E::Paused { .. } | E::Resumed | E::Seeked { .. } | E::Stopped
    );
    if !relevant || !setting(s).share {
        return;
    }
    // Another zone's event matters only when a song starts there (the owner moved)
    let started = matches!(event, E::TrackStarted { .. });
    if !started && written(s).is_some_and(|w| w.zone != zone) {
        return;
    }
    if !crate::listens::plays_are_the_owners(s, zone) {
        return;
    }
    let s = s.clone();
    let zone = zone.to_string();
    tokio::spawn(async move {
        if let Err(e) = reconcile(&s, &zone).await {
            stop_sharing(&s, &e);
        }
    });
}

/// What the zone does now, as the post's next version: `None` when nothing (shareable) plays.
fn wanted(s: &NodeState, zone: &str, now: i64) -> Option<(String, i64, Option<i64>, Option<i64>)> {
    let st = s.zones.get(&pm_zones::ZoneId::new(zone))?.state();
    if st.transport == pm_zones::Transport::Stopped {
        return None;
    }
    let np = st.now_playing?;
    let song = canonical(s, &np.item.uri)?;
    let started_at = now - (np.position_ms as i64) * 1000;
    let paused_at = (st.transport == pm_zones::Transport::Paused).then_some(now);
    Some((song, started_at, paused_at, np.duration_ms.map(|d| d as i64)))
}

/// Write the version of the owner's listening post that says what `zone` does now, if it differs
/// from the last one written.
pub(crate) async fn reconcile(s: &Arc<NodeState>, zone: &str) -> Result<(), String> {
    let _turn = writer().lock().await;
    let share = setting(s);
    let now = pm_pubky::ids::now_micros();
    let last = written(s);
    let Some((song, started_at, paused_at, duration_ms)) =
        share.share.then(|| wanted(s, zone, now)).flatten()
    else {
        // Stopped (or sharing off): the session ends
        if let Some(w) = last.filter(|w| w.zone == zone || !share.share) {
            end(s, &w, now).await?;
        }
        return Ok(());
    };
    // Unchanged: the same song, about the same place, still well inside its expiry
    if let Some(w) = &last {
        let same = w.zone == zone
            && w.song == song
            && (w.started_at - started_at).abs() < 3_000_000
            && w.paused_at.is_some() == paused_at.is_some()
            && now < w.expires_at - 15 * MINUTE;
        if same {
            return Ok(());
        }
    }
    let sync = s.sync_handle().ok_or("not signed in with Pubky")?;
    let key = raw_key(sync.z32()).to_string();
    let owner = pubky_music_specs::PubkyId::try_from(key.as_str()).map_err(|e| e.to_string())?;
    // Keep the session's post unless it ran out (a day, or it expired meanwhile)
    let keep =
        last.filter(|w| now < w.expires_at && now < w.session_at + LIVE_WINDOW_MAX_MICROS - 60 * MINUTE);
    let session_at = keep.as_ref().map_or(started_at.min(now), |w| w.session_at);
    let song_end = duration_ms.map_or(now + 60 * MINUTE, |d| started_at + d * 1000 + 15 * MINUTE);
    let expires_at = song_end
        .max(now + 5 * MINUTE)
        .min(started_at + LIVE_WINDOW_MAX_MICROS)
        .min(session_at + LIVE_WINDOW_MAX_MICROS);
    let content = MusicListeningContent {
        started_at,
        expires_at: Some(expires_at),
        paused_at,
        duration_ms,
        name: share.name.clone().filter(|n| !n.trim().is_empty()),
        ..Default::default()
    };
    let post = pubky_music_specs::listening_post(&content, song.clone());
    let (id, edit, path) = version(&post, keep.as_ref(), &owner, now)?;
    sync.homeserver()
        .write_object(sync.session(), &path, &post)
        .await
        .map_err(|e| format!("your homeserver: {e}"))?;
    store_written(
        s,
        Some(&Written {
            id,
            edit,
            zone: zone.to_string(),
            song,
            session_at,
            started_at,
            paused_at,
            expires_at,
        }),
    );
    Ok(())
}

/// The path of the next version: an edit of the session's post, or a new post.
fn version(
    post: &MusicPost,
    keep: Option<&Written>,
    owner: &pubky_music_specs::PubkyId,
    now: i64,
) -> Result<(String, String, String), String> {
    match keep {
        Some(w) => {
            let v = pubky_music_specs::edit_version(post, &w.id, &w.edit, Root::Pub, owner)?;
            Ok((v.id, v.edit_id, v.path))
        }
        None => {
            let id = pm_pubky::v1::id_of(now);
            let ctx = pubky_music_specs::ValidationCtx { root: Root::Pub };
            pubky_social_specs::traits::Validatable::validate(post, Some(&id), &ctx)
                .map_err(|e| e.to_string())?;
            post.check_references(&ctx, Some(owner))?;
            let path = MusicPost::version_path(MUSIC_NAMESPACE, Root::Pub, &id, &id, None);
            Ok((id.clone(), id, path))
        }
    }
}

/// The session ends: a last version with `ended_at`, and the next play starts a new post.
async fn end(s: &NodeState, w: &Written, now: i64) -> Result<(), String> {
    store_written(s, None);
    if now >= w.expires_at {
        return Ok(()); // readers stopped showing it already
    }
    let sync = s.sync_handle().ok_or("not signed in with Pubky")?;
    let key = raw_key(sync.z32()).to_string();
    let owner = pubky_music_specs::PubkyId::try_from(key.as_str()).map_err(|e| e.to_string())?;
    let content = MusicListeningContent {
        started_at: w.started_at,
        expires_at: Some(w.expires_at.max(now + 1)),
        paused_at: w.paused_at,
        ended_at: Some(now.max(w.started_at)),
        name: setting(s).name.filter(|n| !n.trim().is_empty()),
        ..Default::default()
    };
    let post = pubky_music_specs::listening_post(&content, w.song.clone());
    let v = pubky_music_specs::edit_version(&post, &w.id, &w.edit, Root::Pub, &owner)?;
    sync.homeserver()
        .write_object(sync.session(), &v.path, &post)
        .await
        .map_err(|e| format!("your homeserver: {e}"))
}

/// A write the homeserver refused: sharing turns off, once, instead of retrying.
fn stop_sharing(s: &NodeState, why: &str) {
    tracing::warn!(error = %why, "the listening post could not be written; sharing turns off");
    let mut set = setting(s);
    set.share = false;
    if let Err(e) = s
        .library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(&set).expect("serializes"))
    {
        tracing::warn!(error = %e, "turning sharing off");
    }
}

#[derive(Debug, Serialize)]
pub struct ShareView {
    pub share: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The session's `pubky://…/pub/music/v1/posts/<id>` while one runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<String>,
}

fn view(s: &NodeState) -> ShareView {
    let set = setting(s);
    let now = pm_pubky::ids::now_micros();
    ShareView {
        share: set.share,
        name: set.name,
        post: written(s)
            .filter(|w| now < w.expires_at)
            .zip(s.sync_handle())
            .map(|(w, sync)| format!("pubky://{}/pub/music/v1/posts/{}", raw_key(sync.z32()), w.id)),
    }
}

fn owner_only(c: &crate::accounts::Caller) -> Result<(), ApiError> {
    if c.role != crate::accounts::Role::Owner {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Sharing what you listen to is the owner's for now: it is a post on the owner's homeserver",
        ));
    }
    Ok(())
}

/// `GET /v1/me/listening`.
pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<crate::accounts::Caller>,
) -> ApiResult<ShareView> {
    owner_only(&c)?;
    Ok(Json(view(&s)))
}

#[derive(Debug, Deserialize)]
pub struct ShareUpdate {
    pub share: Option<bool>,
    /// The session's name; empty clears it.
    pub name: Option<String>,
}

/// `PUT /v1/me/listening {share?, name?}`: turning it on needs a Pubky sign-in; turning it off
/// ends the session's post.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<crate::accounts::Caller>,
    Json(u): Json<ShareUpdate>,
) -> ApiResult<ShareView> {
    owner_only(&c)?;
    let mut set = setting(&s);
    if let Some(n) = u.name {
        let n = pubky_social_specs::frozen_trim(&n).to_string();
        if pubky_social_specs::code_point_len(&n) > pubky_music_specs::LIVE_NAME_MAX_LENGTH {
            return Err(ApiError::bad_request(format!(
                "A name is at most {} characters",
                pubky_music_specs::LIVE_NAME_MAX_LENGTH
            )));
        }
        set.name = (!n.is_empty()).then_some(n);
    }
    if let Some(on) = u.share {
        if on && s.sync_handle().is_none() {
            return Err(ApiError::new(
                StatusCode::PRECONDITION_FAILED,
                "Sign in with Pubky to share what you listen to: it is a post on your homeserver",
            ));
        }
        set.share = on;
    }
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(&set).expect("serializes"))?;
    // Off: the session ends now; on: what plays now is shared at once
    let zone = written(&s).map(|w| w.zone).or_else(|| {
        s.zones
            .list()
            .into_iter()
            .find(|z| z.transport == pm_zones::Transport::Playing)
            .map(|z| z.id.0.clone())
    });
    if let Some(zone) = zone {
        if set.share && !crate::listens::plays_are_the_owners(&s, &zone) {
            return Ok(Json(view(&s)));
        }
        if let Err(e) = reconcile(&s, &zone).await {
            stop_sharing(&s, &e);
            return Err(ApiError::new(StatusCode::BAD_GATEWAY, e));
        }
    }
    Ok(Json(view(&s)))
}
