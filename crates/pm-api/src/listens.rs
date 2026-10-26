//! Listen history (owner, 2026-10-26): one music `listen` post per play, where the owner chose
//! (`listens.history`, Settings > You, kept with the homeserver settings copy):
//!
//! - `off`: nothing is written;
//! - `private` (the default): `/priv/music/v1/posts/`;
//! - `public`: `/pub/music/v1/posts/`, for everyone to see.
//!
//! Changing the setting does not move older listens. `POST /v1/me/listens/visibility` moves the
//! whole history (the next sync writes each listen under the new root and deletes the old copy),
//! and `DELETE /v1/me/listens` deletes it here and on the homeserver; listens another node wrote
//! before that time are deleted when the sync finds them.
//!
//! `GET /v1/me/listens` reads the node's copy of the history, which the sync fills from the
//! homeserver (listens other nodes and apps wrote), together with the plays this node kept to
//! itself (`visibility: node`). The node's own play counts (Home's recently and most played)
//! keep working whatever the setting.
//!
// TODO(pinakes): other people's public listens ("who played this", a friend's history) come from
// Pinakes, which indexes `/pub/music/v1/posts/`; the node does not read them from homeservers.

use crate::playlists::store;
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::{Library, MusicPostRow, PlayEntry};
use pm_model::{MusicUri, Track};
use pm_pubky::schemas::post::TYPE_ACTIVITY;
use pm_pubky::{Activity, ActivitySource, MusicContent, MusicPost, Visibility};
use std::collections::HashMap;
use std::sync::Arc;

/// Where listens go: `off`, `private` or `public`.
const SETTING: &str = "listens.history";
/// When the owner last deleted the history (microseconds): older listens the sync still finds
/// on the homeserver are deleted, not imported.
const CLEARED: &str = "listens.cleared_before";
/// A node play and a listen post this close (seconds) are the same play.
const TWIN_SECS: i64 = 5;

pub(crate) fn history(lib: &Library) -> ListenHistory {
    lib.get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(serde_json::Value::String(v)).ok())
        .unwrap_or_default()
}

pub(crate) fn cleared_before(lib: &Library) -> Option<i64> {
    lib.get_setting(CLEARED).ok().flatten()?.parse().ok()
}

/// Record one play as a listen post where the setting says; `None` when the history is off.
/// `played_at` (microseconds) defaults to now. A listen with the same id (the same microsecond)
/// is kept as it is, so an import that runs twice writes nothing twice.
pub(crate) fn record(
    lib: &Library,
    uri: &MusicUri,
    source: ActivitySource,
    duration_ms: Option<u64>,
    played_at: Option<i64>,
) -> Result<Option<String>, ApiError> {
    let visibility = match history(lib) {
        ListenHistory::Off => return Ok(None),
        ListenHistory::Private => Visibility::Private,
        ListenHistory::Public => Visibility::Public,
    };
    let mut a = Activity::new(uri, source, duration_ms);
    if let Some(t) = played_at {
        a.played_at = t;
    }
    let post = MusicPost::new(MusicContent::Activity(a));
    let id = post.create_id();
    post.validate(Some(&id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    if lib.get_music_post(&id)?.is_none() {
        store(lib, &post, &id, visibility)?;
    }
    Ok(Some(id))
}

/// A zone started an item: a listen of a library song with a music URL (`track:<id>`), or of a
/// service track the queue holds by its link. Radio streams and songs without an identity stay
/// out (constraint 4); the node's own play count is kept elsewhere.
pub(crate) fn record_zone_play(lib: &Library, uri: &str, duration_ms: Option<u64>) {
    let (music, source, duration) = match uri
        .strip_prefix(crate::sources::TRACK_SCHEME)
        .and_then(|r| r.parse::<i64>().ok())
    {
        Some(id) => {
            let Some(t) = lib.get_track(pm_model::TrackId(id)).ok().flatten() else {
                return;
            };
            let Some(u) = t.publishable_uri().cloned() else {
                return;
            };
            (u, ActivitySource::Local, t.duration_ms.or(duration_ms))
        }
        None => match MusicUri::parse(uri) {
            Ok(u) => {
                let source = match &u {
                    MusicUri::Service(l) => ActivitySource::Service {
                        name: l.service.label().to_string(),
                    },
                    _ => ActivitySource::Unknown,
                };
                (u, source, duration_ms)
            }
            Err(_) => return,
        },
    };
    if let Err(e) = record(lib, &music, source, duration, None) {
        tracing::warn!(error = %e.message, "recording a listen");
    }
}

/// A listen post row as a history entry, joined to the library.
fn listen_entry(lib: &Library, row: &MusicPostRow) -> Option<ListenEntry> {
    let post = serde_json::from_str::<pubky_app_specs::PubkyAppPost>(&row.json).ok()?;
    let music = MusicPost::from_post(post, Some(&row.id)).ok()??;
    let a = music.as_activity()?;
    Some(ListenEntry {
        id: Some(row.id.clone()),
        uri: a.uri.clone(),
        played_at: a.played_at,
        duration_ms: a.duration_ms,
        source: Some(a.source_kind.clone()),
        zone: None,
        track: track_of(lib, &a.uri),
        visibility: row.visibility.clone(),
        synced: row.synced_at.is_some(),
    })
}

fn track_of(lib: &Library, uri: &str) -> Option<Track> {
    let m = MusicUri::parse(uri).ok()?;
    lib.tracks_by_canonical(&m).ok()?.into_iter().next()
}

/// A play the node recorded, as a history entry.
fn play_entry(p: PlayEntry) -> ListenEntry {
    ListenEntry {
        id: None,
        uri: p.track.publishable_uri().map(|u| u.to_url()).unwrap_or_else(|| {
            format!(
                "{}{}",
                crate::sources::TRACK_SCHEME,
                p.track.id.map(|i| i.0).unwrap_or(0)
            )
        }),
        played_at: p.played_at * 1_000_000,
        duration_ms: p.track.duration_ms,
        source: Some(ActivitySource::Local),
        zone: p.zone,
        track: Some(p.track),
        visibility: "node".into(),
        synced: false,
    }
}

/// Whether a node play is also a listen post (the same song within [`TWIN_SECS`]).
fn has_twin(lib: &Library, p: &PlayEntry) -> bool {
    let Some(uri) = p.track.publishable_uri().map(|u| u.to_url()) else {
        return false;
    };
    let t = p.played_at * 1_000_000;
    let w = TWIN_SECS * 1_000_000;
    lib.music_posts_between(TYPE_ACTIVITY, t - w, t + w)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| listen_entry(lib, r))
        .any(|e| e.uri == uri)
}

/// One page of the history, newest first.
pub(crate) fn page(lib: &Library, before: Option<i64>, limit: u32) -> Result<ListenPageResult, ApiError> {
    let limit = limit.clamp(1, 500);
    let mut out: Vec<ListenEntry> = lib
        .music_posts_page(TYPE_ACTIVITY, before, limit)?
        .iter()
        .filter_map(|r| listen_entry(lib, r))
        .collect();
    // A play at second s is before `before` (microseconds) when s < ceil(before / 1e6)
    let before_secs = before.map(|b| b.div_euclid(1_000_000) + i64::from(b.rem_euclid(1_000_000) != 0));
    for p in lib.plays_before(before_secs, limit)? {
        if !has_twin(lib, &p) {
            out.push(play_entry(p));
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.played_at));
    out.truncate(limit as usize);
    let next = (out.len() == limit as usize)
        .then(|| out.last().map(|e| e.played_at))
        .flatten();
    Ok(ListenPageResult { listens: out, next })
}

/// Top songs and artists of the history since `since` (microseconds).
pub(crate) fn stats(
    lib: &Library,
    period: &str,
    since: Option<i64>,
    limit: usize,
) -> Result<ListenStats, ApiError> {
    let listens: Vec<ListenEntry> = lib
        .music_posts_between(TYPE_ACTIVITY, since.unwrap_or(i64::MIN), i64::MAX)?
        .iter()
        .filter_map(|r| listen_entry(lib, r))
        .collect();
    // Node plays without a listen twin, matched in memory
    let mut twins: HashMap<&str, Vec<i64>> = HashMap::new();
    for l in &listens {
        twins
            .entry(l.uri.as_str())
            .or_default()
            .push(l.played_at / 1_000_000);
    }
    let since_secs = since.map(|t| t.div_euclid(1_000_000));
    let plays: Vec<ListenEntry> = lib
        .plays_before(None, u32::MAX)?
        .into_iter()
        .filter(|p| since_secs.is_none_or(|s| p.played_at >= s))
        .filter(|p| {
            let uri = p.track.publishable_uri().map(|u| u.to_url());
            !uri.as_deref()
                .and_then(|u| twins.get(u))
                .is_some_and(|ts| ts.iter().any(|t| (t - p.played_at).abs() <= TWIN_SECS))
        })
        .map(play_entry)
        .collect();
    let all: Vec<&ListenEntry> = listens.iter().chain(plays.iter()).collect();
    let mut by_track: HashMap<&str, (u64, Option<&Track>)> = HashMap::new();
    let mut by_artist: HashMap<String, (u64, Option<i64>)> = HashMap::new();
    for e in &all {
        let slot = by_track.entry(e.uri.as_str()).or_insert((0, None));
        slot.0 += 1;
        if slot.1.is_none() {
            slot.1 = e.track.as_ref();
        }
        if let Some(t) = &e.track {
            for a in &t.artists {
                let slot = by_artist.entry(a.name.clone()).or_insert((0, a.id));
                slot.0 += 1;
            }
        }
    }
    let mut top_tracks: Vec<TopTrack> = by_track
        .into_iter()
        .map(|(uri, (plays, track))| TopTrack {
            uri: uri.to_string(),
            track: track.cloned(),
            plays,
        })
        .collect();
    top_tracks.sort_by(|a, b| b.plays.cmp(&a.plays).then_with(|| a.uri.cmp(&b.uri)));
    top_tracks.truncate(limit);
    let mut top_artists: Vec<TopArtist> = by_artist
        .into_iter()
        .map(|(name, (plays, artist_id))| TopArtist {
            name,
            artist_id,
            plays,
        })
        .collect();
    top_artists.sort_by(|a, b| b.plays.cmp(&a.plays).then_with(|| a.name.cmp(&b.name)));
    top_artists.truncate(limit);
    Ok(ListenStats {
        period: period.to_string(),
        since,
        listens: all.len() as u64,
        top_tracks,
        top_artists,
    })
}

/// The setting is one of the owner's homeserver settings: save the copy now, not only after the
/// next sync.
fn back_up_settings(s: &Arc<NodeState>) {
    if let Some(sync) = s.sync_handle() {
        let s = s.clone();
        tokio::spawn(async move { sync.backup_settings(&s).await });
    }
}

// ---- handlers ----

/// `GET /v1/me/listens/settings`.
pub(crate) async fn get_settings(State(s): State<Arc<NodeState>>) -> ApiResult<ListenSettings> {
    let lib = s.library.lock().expect("library poisoned");
    let (private, public) = lib.music_post_counts(TYPE_ACTIVITY)?;
    Ok(Json(ListenSettings {
        history: history(&lib),
        private,
        public,
    }))
}

/// `PUT /v1/me/listens/settings {history}`: where new listens go. Older ones stay where they are.
pub(crate) async fn put_settings(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<SetListenSettings>,
) -> ApiResult<ListenSettings> {
    {
        let lib = s.library.lock().expect("library poisoned");
        let v = serde_json::to_value(body.history).expect("serializes");
        lib.set_setting(SETTING, v.as_str().unwrap_or("private"))?;
    }
    back_up_settings(&s);
    get_settings(State(s)).await
}

/// `GET /v1/me/listens?limit=&before=`.
pub(crate) async fn list(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<ListenPage>,
) -> ApiResult<ListenPageResult> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, q.before, q.limit.unwrap_or(50))?))
}

/// `GET /v1/me/listens/stats?period=week|month|year|all&limit=`.
pub(crate) async fn get_stats(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<ListenStatsQuery>,
) -> ApiResult<ListenStats> {
    let period = q.period.unwrap_or_else(|| "month".into()).to_ascii_lowercase();
    let days = match period.as_str() {
        "week" => Some(7),
        "month" => Some(30),
        "year" => Some(365),
        "all" => None,
        other => {
            return Err(ApiError::bad_request(format!(
                "period {other:?}: use week, month, year or all"
            )))
        }
    };
    let since = days.map(|d: i64| pm_pubky::ids::now_micros() - d * 86_400_000_000);
    let limit = q.limit.unwrap_or(10).clamp(1, 100) as usize;
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(stats(&lib, &period, since, limit)?))
}

/// `POST /v1/me/listens/visibility {public}`: move the whole history under `/pub/` or `/priv/`.
pub(crate) async fn move_all(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<MoveListens>,
) -> ApiResult<MovedListens> {
    let to = if body.public { "public" } else { "private" };
    let moved = s
        .library
        .lock()
        .expect("library poisoned")
        .set_music_posts_visibility(TYPE_ACTIVITY, to)?;
    s.notify_library();
    Ok(Json(MovedListens { moved }))
}

/// `DELETE /v1/me/listens?local=`: delete the history here and, with the next sync, on the
/// homeserver (and listens other nodes wrote before now when the sync finds them).
pub(crate) async fn delete_all(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<DeleteListens>,
) -> ApiResult<DeletedListens> {
    let now = pm_pubky::ids::now_micros();
    let (deleted, plays_cleared) = {
        let lib = s.library.lock().expect("library poisoned");
        let deleted = lib.delete_music_posts_of_kind(TYPE_ACTIVITY, now)?;
        lib.set_setting(CLEARED, &now.to_string())?;
        let plays = if q.local == Some(true) {
            lib.clear_plays()?
        } else {
            0
        };
        (deleted, plays)
    };
    back_up_settings(&s);
    s.notify_library();
    Ok(Json(DeletedListens {
        deleted,
        plays_cleared,
    }))
}
