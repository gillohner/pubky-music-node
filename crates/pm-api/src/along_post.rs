//! Listening along with someone's Pubky listening post (listening together, L4; owner decision
//! 2026-10-29): their public `listening` post, played on one of your zones from your own source
//! (the library, else a connected service), at the post's position, following each new version.
//! Nothing streams from their node.
//!
//! - **Finding the post:** their newest `listening` post among their posts of the last day,
//!   newest first. Listens and reviews are skipped, and what each post is stays known, so a later
//!   look reads only the posts it hasn't seen.
//! - **Following:** the post's newest version is read every few seconds. A new song plays here
//!   from your source at their position; a pause pauses; a resume or a seek moves to where they
//!   are. A song none of your sources has leaves the zone stopped until their next one. When
//!   they stop (or the post runs out), the zone pauses and following ends.
//! - **Leaving:** `DELETE`, or changing the zone's queue yourself; what plays here stays.
//!
//! In memory only: a restart forgets who follows what.

use crate::accounts::{self, Caller, Role};
use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use pm_pubky::homeserver::NodeHomeserver;
use pm_zones::{QueueItem, Transport, Zone, ZoneId};
use pubky_music_specs::{MusicListeningContent, MusicPost, MusicPostKind, LIVE_WINDOW_MAX_MICROS};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const POSTS: &str = "/pub/music/v1/posts/";
/// How often a followed post is read again.
const EVERY: Duration = Duration::from_secs(4);
/// How many of a person's newest posts are looked at for their listening post.
const SCAN: u16 = 60;
/// A position this far from theirs is moved to theirs.
const DRIFT_MS: i64 = 3_000;

/// What the node knows of listening posts: what each post seen is, and who follows what.
#[derive(Default)]
pub struct AlongPosts {
    /// `key/id` → whether that post is a `listening`.
    kinds: Mutex<HashMap<String, bool>>,
    /// Who follows what, by zone.
    follows: Mutex<HashMap<String, Follow>>,
    next: AtomicU64,
}

/// One zone following a post.
#[derive(Debug, Clone)]
struct Follow {
    zone: String,
    key: String,
    id: String,
    edit: String,
    /// Who here follows it (their key, as presence names them).
    by: String,
    /// Their display name.
    who: String,
    /// The session's name, or `<their name> is listening`.
    name: String,
    song: String,
    status: &'static str,
    /// When the post stops being live without a newer version.
    expires: i64,
    /// The zone's queue as following left it: another queue means someone here moved on.
    queue_version: u64,
    generation: u64,
}

/// A zone following a post, as the page shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Following {
    pub zone: String,
    /// Whose post: their key.
    pub key: String,
    /// The post's `pubky://…/pub/music/v1/posts/<id>` (comments take it as their parent).
    pub post: String,
    /// Their display name.
    pub who: String,
    /// The session's name, or `<their name> is listening`.
    pub name: String,
    /// The song they play now (its link).
    pub song: String,
    /// `playing`, `paused`, `missing` (none of your sources has it: the zone waits for their next
    /// song) or `ended` (they stopped).
    pub status: &'static str,
    /// You follow it (you may leave it).
    pub mine: bool,
}

fn post_uri(key: &str, id: &str) -> String {
    format!("pubky://{key}/pub/music/v1/posts/{id}")
}

fn view(f: &Follow, me: Option<&str>) -> Following {
    Following {
        zone: f.zone.clone(),
        key: f.key.clone(),
        post: post_uri(&f.key, &f.id),
        who: f.who.clone(),
        name: f.name.clone(),
        song: f.song.clone(),
        status: f.status,
        mine: me == Some(f.by.as_str()),
    }
}

/// One version of a person's listening post.
#[derive(Debug, Clone)]
pub(crate) struct Read {
    pub id: String,
    pub edit: String,
    pub song: String,
    pub content: MusicListeningContent,
}

impl Read {
    fn expires(&self) -> i64 {
        let c = &self.content;
        let end = c
            .expires_at
            .unwrap_or(c.started_at.saturating_add(LIVE_WINDOW_MAX_MICROS));
        c.ended_at.map_or(end, |e| e.min(end))
    }
}

/// The last segment of a listed path (`…/posts/<id>/` → `<id>`).
fn leaf(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or_default()
}

/// The newest version of post `id` of `key`, when it is a music post: `(editId, post)`.
async fn newest_version(h: &NodeHomeserver, key: &str, id: &str) -> Option<(String, MusicPost)> {
    let dir = format!("{POSTS}{id}/");
    let found = h.newest_public_paths(key, &dir, 1, true).await.ok()?;
    let path = found.into_iter().next()?;
    let path = if path.starts_with('/') {
        path
    } else {
        format!("{dir}{}", leaf(&path))
    };
    let (_, edit) = pm_pubky::v1::parse_version(&path)?;
    let bytes = h
        .get_bytes(key, &path, pm_pubky::Visibility::Public, None)
        .await
        .ok()?;
    let post: MusicPost = serde_json::from_slice(&bytes).ok()?;
    Some((edit, post))
}

/// A listening post's version, checked: its content and the song it embeds.
fn read_of(id: &str, edit: String, post: &MusicPost) -> Option<Read> {
    let content = pubky_music_specs::read_listening(post).ok()?;
    let song = post
        .embed
        .clone()
        .filter(|e| pm_model::MusicUri::parse(e).is_ok())?;
    Some(Read {
        id: id.to_string(),
        edit,
        song,
        content,
    })
}

/// `key`'s newest listening post (live or not), among their posts of the last day.
pub(crate) async fn newest_listening(s: &NodeState, key: &str) -> Result<Option<Read>, ApiError> {
    let h = s
        .homeserver()
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("Pubky: {e}")))?;
    let since = pm_pubky::ids::timestamp_id(pm_pubky::ids::now_micros() - LIVE_WINDOW_MAX_MICROS);
    let dirs = h
        .newest_public_paths(key, POSTS, SCAN, true)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("their homeserver: {e}")))?;
    for dir in dirs {
        let id = leaf(&dir).to_string();
        // Newest first: a post older than a day ends the search (it can't be live)
        if id.len() != 13 || id.as_str() < since.as_str() {
            break;
        }
        let k = format!("{key}/{id}");
        if s.along_posts.kinds.lock().expect("along kinds").get(&k) == Some(&false) {
            continue;
        }
        let Some((edit, post)) = newest_version(&h, key, &id).await else {
            continue;
        };
        let listening = post.kind == MusicPostKind::Listening;
        {
            let mut kinds = s.along_posts.kinds.lock().expect("along kinds");
            if kinds.len() > 10_000 {
                kinds.clear();
            }
            kinds.insert(k, listening);
        }
        if listening {
            return Ok(read_of(&id, edit, &post));
        }
    }
    Ok(None)
}

/// A person's name for a listening post: their display name, else a short key.
async fn display_name(s: &NodeState, key: &str) -> String {
    crate::profiles::known_cards(s, &[key.to_string()])
        .await
        .remove(key)
        .map(|c| c.name)
        .unwrap_or_else(|| crate::profiles::short(key))
}

/// What a person is listening to now, as `GET /v1/profiles/{id}/listening` answers it.
#[derive(Debug, Default, Serialize)]
pub struct LiveView {
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<String>,
    /// The session's name, or `<their name> is listening`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// `GET /v1/profiles/{id}/listening`: what they are listening to now, from their listening post
/// (`live: false` when nothing).
pub(crate) async fn listening(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<LiveView> {
    if !crate::profiles::valid_id(&id) {
        return Err(ApiError::bad_request("that isn't a Pubky key"));
    }
    let now = pm_pubky::ids::now_micros();
    let Some(r) = newest_listening(&s, &id)
        .await?
        .filter(|r| r.content.is_live(now))
    else {
        return Ok(Json(LiveView::default()));
    };
    let name = r.content.name_or_generic(&display_name(&s, &id).await);
    let about = crate::discover::describe_uris(&s, std::slice::from_ref(&r.song))
        .await
        .into_iter()
        .next();
    Ok(Json(LiveView {
        live: true,
        post: Some(post_uri(&id, &r.id)),
        name: Some(name),
        title: about.as_ref().and_then(|a| a.title.clone()),
        artist: about.as_ref().and_then(|a| a.artist.clone()),
        image: about.and_then(|a| a.image),
        song: Some(r.song),
        paused: r.content.paused_at.is_some(),
        position_ms: r.content.position_ms(now),
        duration_ms: r.content.duration_ms,
    }))
}

/// The queue items that play `song` here from your own source: the library's song, else a
/// connected service's (what "Play" on its card would play). `None` when nothing here has it.
async fn queue_for(s: &Arc<NodeState>, owner: bool, song: &str) -> Option<Vec<QueueItem>> {
    // The library's own song first: no need to ask anyone
    if let Ok(u) = pm_model::MusicUri::parse(song) {
        let lib = s.library.lock().expect("library poisoned");
        let mine = lib
            .tracks_by_canonical(&u)
            .ok()
            .and_then(|t| t.into_iter().next());
        if let Some(item) = mine.as_ref().and_then(crate::sources::NodeSources::item_for) {
            return Some(vec![item]);
        }
    }
    let q = crate::types::DiscoverQuery {
        uri: Some(song.to_string()),
        limit: None,
        window: None,
        cursor: None,
        type_: None,
        q: None,
    };
    let plan = crate::discover::play_plan(State(s.clone()), Query(q))
        .await
        .ok()?
        .0;
    let mut items: Vec<QueueItem> = Vec::new();
    if let Some(id) = plan.track_ids.first() {
        let lib = s.library.lock().expect("library poisoned");
        let track = lib.get_track(pm_model::TrackId(*id)).ok().flatten()?;
        items.extend(crate::sources::NodeSources::item_for(&track));
    } else {
        items = plan.items;
    }
    if !owner {
        items.retain_mut(|it| accounts::check_listener_item(it).is_ok());
    }
    (!items.is_empty()).then_some(items)
}

/// Make `zone` do what version `r` says, now. `fresh`: load the song even when it is the one
/// following has (the first time, or after the zone was left without it).
async fn apply(s: &Arc<NodeState>, owner: bool, zone: &Zone, f: &mut Follow, r: &Read, fresh: bool) {
    let now = pm_pubky::ids::now_micros();
    f.expires = r.expires();
    let Some(pos) = r.content.position_ms(now) else {
        // They stopped (or it ran out): the zone pauses where it is
        if zone.state().transport == Transport::Playing {
            zone.pause();
        }
        f.status = "ended";
        return;
    };
    let paused = r.content.paused_at.is_some();
    let st = zone.state();
    let near_end = r.content.duration_ms.is_some_and(|d| pos >= d - 1_000);
    let reload = fresh
        || r.song != f.song
        || f.status == "missing"
        || (st.transport == Transport::Stopped && !near_end);
    f.song = r.song.clone();
    if reload {
        match queue_for(s, owner, &r.song).await {
            Some(items) => {
                let transport = if paused {
                    Transport::Paused
                } else {
                    Transport::Playing
                };
                crate::load_zone(
                    zone,
                    items,
                    Some(0),
                    pos as u64,
                    transport,
                    pm_zones::Repeat::Off,
                    false,
                )
                .await;
                f.status = if paused { "paused" } else { "playing" };
            }
            None => {
                zone.stop();
                f.status = "missing";
            }
        }
        return;
    }
    let here = st.now_playing.as_ref().map(|n| n.position_ms as i64);
    if here.is_none_or(|h| (h - pos).abs() > DRIFT_MS) && !near_end {
        zone.seek_ms(pos as u64);
    }
    if paused {
        if st.transport == Transport::Playing {
            zone.pause();
        }
        f.status = "paused";
    } else {
        if st.transport == Transport::Paused {
            zone.play();
        }
        f.status = "playing";
    }
}

/// The follow on `zone` while it is still generation `generation`.
fn current(s: &NodeState, zone: &str, generation: u64) -> Option<Follow> {
    s.along_posts
        .follows
        .lock()
        .expect("along follows")
        .get(zone)
        .filter(|f| f.generation == generation)
        .cloned()
}

/// Store `f` back, unless it was left (or replaced) meanwhile.
fn store(s: &NodeState, f: Follow) -> bool {
    let mut all = s.along_posts.follows.lock().expect("along follows");
    match all.get(&f.zone) {
        Some(x) if x.generation == f.generation => {
            all.insert(f.zone.clone(), f);
            true
        }
        _ => false,
    }
}

fn forget(s: &NodeState, zone: &str, generation: u64) {
    let mut all = s.along_posts.follows.lock().expect("along follows");
    if all.get(zone).is_some_and(|f| f.generation == generation) {
        all.remove(zone);
    }
}

/// Read the followed post every few seconds and play along, until it ends or is left.
fn follow_on(s: Arc<NodeState>, owner: bool, zone_id: String, generation: u64) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(EVERY).await;
            let Some(mut f) = current(&s, &zone_id, generation) else {
                return;
            };
            let Some(zone) = s.zones.get(&ZoneId::new(&zone_id)) else {
                forget(&s, &zone_id, generation);
                return;
            };
            // Someone here put something else on: following ends, what plays stays
            if zone.state().queue_version != f.queue_version {
                forget(&s, &zone_id, generation);
                return;
            }
            let now = pm_pubky::ids::now_micros();
            let newest = match s.homeserver() {
                Ok(h) => newest_version(&h, &f.key, &f.id).await,
                Err(_) => None,
            };
            let r = newest
                .filter(|(edit, _)| *edit != f.edit)
                .and_then(|(edit, post)| read_of(&f.id, edit, &post));
            match r {
                Some(r) => {
                    apply(&s, owner, &zone, &mut f, &r, false).await;
                    f.edit = r.edit;
                }
                // No newer version: it may have run out
                None if now >= f.expires => {
                    if zone.state().transport == Transport::Playing {
                        zone.pause();
                    }
                    f.status = "ended";
                }
                None => continue,
            }
            f.queue_version = zone.state().queue_version;
            let ended = f.status == "ended";
            if !store(&s, f) || ended {
                return;
            }
        }
    });
}

#[derive(Debug, Deserialize)]
pub struct FollowPost {
    /// Whose listening post (their key).
    pub key: String,
    /// The zone that plays along.
    pub zone: String,
}

/// `POST /v1/presence/post {key, zone}`: listen along with someone's listening post on `zone`,
/// from your own source.
pub(crate) async fn follow(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Json(body): Json<FollowPost>,
) -> ApiResult<Following> {
    let me = crate::presence::who(&c).ok_or_else(|| accounts::refused("that is for people on this node"))?;
    if !crate::profiles::valid_id(&body.key) {
        return Err(ApiError::bad_request("that isn't a Pubky key"));
    }
    if body.key == me {
        return Err(ApiError::bad_request("that is your own listening"));
    }
    if !accounts::may_use_zone(&s, &c, &body.zone) {
        return Err(accounts::refused(
            "the owner has not opened that zone to listeners",
        ));
    }
    let zone = s
        .zones
        .get(&ZoneId::new(&body.zone))
        .ok_or_else(|| ApiError::not_found("no such zone"))?;
    if s.presence.sources(&body.zone).is_some() {
        return Err(ApiError::bad_request(
            "that zone is a listen-along group: pick one of your own",
        ));
    }
    // Someone else's music there (whatever they share of it) is not stopped for this
    if crate::presence::someone_else_plays(&s, &body.zone, &me) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "someone else is listening on that zone: pick another",
        ));
    }
    let now = pm_pubky::ids::now_micros();
    let r = newest_listening(&s, &body.key)
        .await?
        .filter(|r| r.content.is_live(now))
        .ok_or_else(|| ApiError::not_found("they aren't listening now"))?;
    let owner = c.role == Role::Owner;
    let who = display_name(&s, &body.key).await;
    let generation = s.along_posts.next.fetch_add(1, Ordering::Relaxed) + 1;
    let mut f = Follow {
        zone: body.zone.clone(),
        key: body.key.clone(),
        id: r.id.clone(),
        edit: r.edit.clone(),
        by: me.clone(),
        name: r.content.name_or_generic(&who),
        who,
        song: String::new(),
        status: "playing",
        expires: r.expires(),
        queue_version: 0,
        generation,
    };
    apply(&s, owner, &zone, &mut f, &r, true).await;
    f.queue_version = zone.state().queue_version;
    crate::listens::zone_changed_by(&s, &body.zone, owner);
    crate::presence::played(&s, &body.zone, &c);
    let out = view(&f, Some(&me));
    s.along_posts
        .follows
        .lock()
        .expect("along follows")
        .insert(body.zone.clone(), f);
    follow_on(s.clone(), owner, body.zone, generation);
    Ok(Json(out))
}

/// `GET /v1/presence/post`: the zones you may use that follow someone's listening post.
pub(crate) async fn list(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
) -> ApiResult<Vec<Following>> {
    let me = crate::presence::who(&c);
    let mut out: Vec<Following> = s
        .along_posts
        .follows
        .lock()
        .expect("along follows")
        .values()
        .filter(|f| accounts::may_use_zone(&s, &c, &f.zone))
        .map(|f| view(f, me.as_deref()))
        .collect();
    out.sort_by(|a, b| a.zone.cmp(&b.zone));
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct LeavePost {
    pub zone: String,
}

/// `DELETE /v1/presence/post?zone=`: stop listening along there (what plays stays). Whoever
/// follows it, or the owner.
pub(crate) async fn leave(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Query(q): Query<LeavePost>,
) -> Result<StatusCode, ApiError> {
    let me = crate::presence::who(&c).ok_or_else(|| accounts::refused("that is for people on this node"))?;
    let mut all = s.along_posts.follows.lock().expect("along follows");
    let f = all
        .get(&q.zone)
        .ok_or_else(|| ApiError::not_found("that zone isn't listening along"))?;
    if f.by != me && c.role != Role::Owner {
        return Err(accounts::refused("only who started it or the owner ends it"));
    }
    all.remove(&q.zone);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_listed_path_ends_in_its_id() {
        assert_eq!(super::leaf("/pub/music/v1/posts/0034A0Z4013G0/"), "0034A0Z4013G0");
        assert_eq!(super::leaf("0034A0Z4013G0/"), "0034A0Z4013G0");
        assert_eq!(
            super::leaf("/pub/music/v1/posts/0034A0Z4013G0/0034A0Z4013G1.json"),
            "0034A0Z4013G1.json"
        );
    }
}
