//! Party mode (compare-music-assistant.md #4): the owner opens a party on one zone and shares a
//! link; guests open it on their phones, give a name and add songs from the library. No Pubky
//! account is needed.
//!
//! - **Guest links expire** (1 to 72 hours) and the owner can end a party early; a guest's token
//!   stops with its party.
//! - **The guest section:** guests' songs go right after the current song, after any other
//!   guest songs still waiting, so requests play in the order they came and the owner's queue
//!   follows.
//! - **Limits are checked here, on the node:** songs per guest per hour, and songs a guest may
//!   have waiting at once. (Music Assistant sends its limits to the guest page and checks them
//!   there only.)
//! - **What a guest token opens:** `/v1/party/guest…` and the covers of what it shows. Nothing
//!   else: no transport, no volume, no other zone.
//! - **Shared on Pubky** (`share`, TASK-pinakes-social-ux.md D1): the party is one public music
//!   `session` post on the owner's homeserver (`/pub/music/v1/posts/{id}/{editId}.json`). It embeds
//!   the song playing, names the owner as `host` and the party link as `join`, and gets a new
//!   version when the song changes, never a post per song. It ends when the party does (deleted),
//!   or at its `expires_at`. A session lasts at most a day, so a longer party starts a fresh post
//!   once a day.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use pm_model::TrackId;
use pm_zones::{Insert, Transport, ZoneId};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

const PARTIES: &str = "party.parties";
/// Guest tokens start with this, so the auth layer can tell them apart cheaply.
pub const GUEST_PREFIX: &str = "pmg_";
const MAX_GUESTS: usize = 200;
const HOUR: i64 = 3600;

/// Party reads and writes are read-modify-write on one setting; one at a time.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Guest {
    /// The hash of the guest's token.
    pub token_hash: String,
    pub name: String,
    pub joined_at: i64,
    /// When they added songs (the last hour's).
    #[serde(default)]
    pub adds: Vec<i64>,
    /// Queue item ids of the songs they added.
    #[serde(default)]
    pub items: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Party {
    pub id: String,
    /// The code in the guest link (the owner can show the link again).
    pub code: String,
    pub name: String,
    pub zone: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub adds_per_hour: u32,
    pub waiting_per_guest: u32,
    #[serde(default)]
    pub guests: Vec<Guest>,
    /// Shared on Pubky as a `session` post.
    #[serde(default)]
    pub share: bool,
    /// The session post, once written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post: Option<SharedPost>,
}

/// The session post a shared party is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedPost {
    pub id: String,
    /// The newest version's edit id.
    pub edit: String,
    /// The post's `started_at`, Unix microseconds.
    pub started_at: i64,
    /// The song it embeds now.
    pub song: String,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// Whether a party is running on this zone (its guests add songs to it).
pub(crate) fn open_on(s: &NodeState, zone: &str) -> bool {
    load(s).iter().any(|p| p.zone == zone)
}

/// The parties still running (expired ones are dropped as they are read).
fn load(s: &NodeState) -> Vec<Party> {
    let all: Vec<Party> = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(PARTIES)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let t = now();
    all.into_iter().filter(|p| p.expires_at > t).collect()
}

fn store(s: &NodeState, parties: &[Party]) -> Result<(), ApiError> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(PARTIES, &serde_json::to_string(parties).expect("serializes"))?;
    Ok(())
}

/// The party and guest a guest token belongs to, while the party runs.
pub fn guest_of(s: &NodeState, token: &str) -> Option<(Party, usize)> {
    if !token.starts_with(GUEST_PREFIX) {
        return None;
    }
    let h = hash(token);
    let _g = LOCK.lock().expect("party lock");
    load(s).into_iter().find_map(|p| {
        let i = p.guests.iter().position(|g| g.token_hash == h)?;
        Some((p, i))
    })
}

/// Whether a guest may make this request: only the party routes and covers.
pub fn guest_refusal(method: &axum::http::Method, path: &str) -> Option<&'static str> {
    let path = path.strip_prefix("/v1").unwrap_or(path);
    if path == "/party/guest" || path.starts_with("/party/guest/") {
        return None;
    }
    let is_cover = path
        .strip_prefix("/library/albums/")
        .and_then(|r| r.strip_suffix("/cover"))
        .is_some_and(|id| id.parse::<i64>().is_ok());
    if is_cover && (*method == axum::http::Method::GET || *method == axum::http::Method::HEAD) {
        return None;
    }
    Some("a party guest can only add songs to the party")
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

fn zone_name(s: &NodeState, id: &str) -> Option<String> {
    s.zones.get(&ZoneId::new(id)).map(|z| z.state().name)
}

fn party_url(s: &NodeState, code: &str) -> String {
    format!("{}/party/{code}", s.public_url.trim_end_matches('/'))
}

// ---- the owner ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartyView {
    pub id: String,
    pub name: String,
    pub zone: String,
    pub zone_name: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub adds_per_hour: u32,
    pub waiting_per_guest: u32,
    pub guests: Vec<String>,
    /// Songs guests have added so far.
    pub added: usize,
    pub url: String,
    pub qr_svg: Option<String>,
    /// Shared on Pubky.
    pub shared: bool,
    /// The session post's `pubky://` link, once written (the first song after sharing writes it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<String>,
}

fn view(s: &NodeState, p: &Party) -> PartyView {
    let url = party_url(s, &p.code);
    PartyView {
        id: p.id.clone(),
        name: p.name.clone(),
        zone: p.zone.clone(),
        zone_name: zone_name(s, &p.zone),
        created_at: p.created_at,
        expires_at: p.expires_at,
        adds_per_hour: p.adds_per_hour,
        waiting_per_guest: p.waiting_per_guest,
        guests: p.guests.iter().map(|g| g.name.clone()).collect(),
        added: p.guests.iter().map(|g| g.items.len()).sum(),
        qr_svg: crate::qr_svg(&url),
        url,
        shared: p.share,
        post: p
            .post
            .as_ref()
            .zip(s.sync_handle())
            .map(|(post, sync)| format!("pubky://{}/pub/music/v1/posts/{}", raw_key(sync.z32()), post.id)),
    }
}

/// `GET /v1/party`: the parties running now.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> Json<Vec<PartyView>> {
    let parties = {
        let _g = LOCK.lock().expect("party lock");
        load(&s)
    };
    Json(parties.iter().map(|p| view(&s, p)).collect())
}

#[derive(Debug, Deserialize)]
pub struct NewParty {
    pub zone: String,
    #[serde(default)]
    pub name: Option<String>,
    /// How long the link works, 1 to 72 hours (6 by default).
    #[serde(default)]
    pub hours: Option<u32>,
    /// Songs one guest may add per hour (5 by default, 1 to 100).
    #[serde(default)]
    pub adds_per_hour: Option<u32>,
    /// Songs one guest may have waiting at once (3 by default, 1 to 50).
    #[serde(default)]
    pub waiting_per_guest: Option<u32>,
    /// Share it on Pubky as a listening session (needs a Pubky sign-in).
    #[serde(default)]
    pub share: bool,
}

/// `POST /v1/party {zone, name?, hours?, adds_per_hour?, waiting_per_guest?, share?}`.
pub(crate) async fn create(State(s): State<Arc<NodeState>>, Json(b): Json<NewParty>) -> ApiResult<PartyView> {
    let Some(zname) = zone_name(&s, &b.zone) else {
        return Err(ApiError::not_found(format!("no zone {}", b.zone)));
    };
    let hours = b.hours.unwrap_or(6);
    if !(1..=72).contains(&hours) {
        return Err(ApiError::bad_request("a party lasts 1 to 72 hours"));
    }
    if b.share && s.sync_handle().is_none() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Sign in with Pubky to share a party: it is a post on your homeserver",
        ));
    }
    let adds = b.adds_per_hour.unwrap_or(5);
    let waiting = b.waiting_per_guest.unwrap_or(3);
    if !(1..=100).contains(&adds) || !(1..=50).contains(&waiting) {
        return Err(ApiError::bad_request(
            "adds_per_hour is 1 to 100 and waiting_per_guest 1 to 50",
        ));
    }
    let name = b
        .name
        .map(|n| n.trim().chars().take(60).collect::<String>())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Party on {zname}"));
    let t = now();
    let p = Party {
        id: crate::tidal::random_urlsafe(9),
        code: crate::tidal::random_urlsafe(18),
        name,
        zone: b.zone,
        created_at: t,
        expires_at: t + i64::from(hours) * HOUR,
        adds_per_hour: adds,
        waiting_per_guest: waiting,
        guests: Vec::new(),
        share: b.share,
        post: None,
    };
    {
        let _g = LOCK.lock().expect("party lock");
        let mut all = load(&s);
        all.push(p.clone());
        store(&s, &all)?;
    }
    // A song already playing is the first one the post shows
    if p.share {
        let playing = s
            .zones
            .get(&ZoneId::new(&p.zone))
            .and_then(|z| z.state().now_playing.map(|n| n.item.uri));
        if let Some(uri) = playing {
            if let Err(e) = publish(&s, &p.id, &uri).await {
                tracing::warn!(error = %e, "sharing the party on Pubky");
            }
        }
    }
    let p = {
        let _g = LOCK.lock().expect("party lock");
        load(&s).into_iter().find(|x| x.id == p.id).unwrap_or(p)
    };
    Ok(Json(view(&s, &p)))
}

/// `DELETE /v1/party/{id}`: end a party now; its guests' tokens stop.
pub(crate) async fn end(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let ended = {
        let _g = LOCK.lock().expect("party lock");
        let mut all = load(&s);
        let Some(i) = all.iter().position(|p| p.id == id) else {
            return Err(ApiError::not_found("no such party"));
        };
        let p = all.remove(i);
        store(&s, &all)?;
        p
    };
    // Its session post goes with it
    if let Some(post) = ended.post {
        if let Err(e) = unpublish(&s, &post.id).await {
            tracing::warn!(error = %e, "removing the party's session post");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- shared on Pubky (D1) ----

fn raw_key(z32: &str) -> &str {
    z32.strip_prefix("pubky").filter(|k| k.len() == 52).unwrap_or(z32)
}

/// The canonical URI of what a zone plays: the library song's, or the music link itself.
fn canonical_song(s: &NodeState, uri: &str) -> Option<String> {
    match uri
        .strip_prefix(crate::sources::TRACK_SCHEME)
        .and_then(|r| r.parse::<i64>().ok())
    {
        Some(id) => {
            let lib = s.library.lock().expect("library poisoned");
            let t = lib.get_track(TrackId(id)).ok().flatten()?;
            t.publishable_uri().map(|u| u.to_url())
        }
        None => pm_model::MusicUri::parse(uri).ok().map(|u| u.to_url()),
    }
}

/// A song started on `zone`: a shared party there shows it (a new version of its post).
pub(crate) fn track_started(s: &Arc<NodeState>, zone: &str, uri: &str) {
    let shared = {
        let _g = LOCK.lock().expect("party lock");
        load(s).into_iter().find(|p| p.zone == zone && p.share)
    };
    let Some(p) = shared else { return };
    let s = s.clone();
    let uri = uri.to_string();
    tokio::spawn(async move {
        if let Err(e) = publish(&s, &p.id, &uri).await {
            tracing::warn!(error = %e, "updating the party's session post");
        }
    });
}

/// Write the party's session post for the song at `uri` (a zone's item URI): its first version,
/// a new version when the song changed, or a fresh post once a day.
pub(crate) async fn publish(s: &Arc<NodeState>, party_id: &str, uri: &str) -> Result<(), String> {
    use pubky_music_specs::{MusicPost, Root, LIVE_WINDOW_MAX_MICROS, MUSIC_NAMESPACE};
    let Some(song) = canonical_song(s, uri) else {
        return Ok(()); // nothing a post can name (a file with no identity, a radio stream)
    };
    let sync = s.sync_handle().ok_or("not signed in with Pubky")?;
    let p = {
        let _g = LOCK.lock().expect("party lock");
        load(s).into_iter().find(|p| p.id == party_id)
    };
    let Some(p) = p else { return Ok(()) };
    let micros = pm_pubky::ids::now_micros();
    // Same song: nothing to change
    if p.post
        .as_ref()
        .is_some_and(|x| x.song == song && micros < x.started_at + LIVE_WINDOW_MAX_MICROS - 3_600_000_000)
    {
        return Ok(());
    }
    let key = raw_key(sync.z32()).to_string();
    let owner = pubky_music_specs::PubkyId::try_from(key.as_str()).map_err(|e| e.to_string())?;
    // A day at most per post: past 23 hours a fresh post takes over
    let (keep, stale) = match &p.post {
        Some(x) if micros < x.started_at + LIVE_WINDOW_MAX_MICROS - 3_600_000_000 => (Some(x.clone()), None),
        Some(x) => (None, Some(x.id.clone())),
        None => (None, None),
    };
    let started_at = keep.as_ref().map(|x| x.started_at).unwrap_or(micros);
    let expires_at = (p.expires_at * 1_000_000).min(started_at + LIVE_WINDOW_MAX_MICROS);
    let join = party_url(s, &p.code);
    let post = pubky_music_specs::session(
        started_at,
        Some(expires_at),
        format!("pubky://{key}"),
        Some(p.name.clone()),
        None,
        song.clone(),
    );
    let post = pubky_music_specs::session_joined_at(
        &post,
        (join.starts_with("https://") || join.starts_with("http://")).then_some(join),
    );
    let ctx = pubky_music_specs::ValidationCtx { root: Root::Pub };
    let (id, edit, path) = match &keep {
        Some(x) => {
            let v = pubky_music_specs::edit_version(&post, &x.id, &x.edit, Root::Pub, &owner)?;
            (v.id, v.edit_id, v.path)
        }
        None => {
            let id = pm_pubky::v1::id_of(micros);
            pubky_social_specs::traits::Validatable::validate(&post, Some(&id), &ctx)
                .map_err(|e| e.to_string())?;
            post.check_references(&ctx, Some(&owner))?;
            let path = MusicPost::version_path(MUSIC_NAMESPACE, Root::Pub, &id, &id, None);
            (id.clone(), id, path)
        }
    };
    sync.homeserver()
        .write_object(sync.session(), &path, &post)
        .await
        .map_err(|e| format!("your homeserver: {e}"))?;
    if let Some(old) = stale {
        unpublish(s, &old).await?;
    }
    let _g = LOCK.lock().expect("party lock");
    let mut all = load(s);
    if let Some(x) = all.iter_mut().find(|x| x.id == party_id) {
        x.post = Some(SharedPost {
            id,
            edit,
            started_at,
            song,
        });
        store(s, &all).map_err(|e| e.message)?;
    }
    Ok(())
}

/// Delete every version of a session post.
async fn unpublish(s: &NodeState, id: &str) -> Result<(), String> {
    let sync = s.sync_handle().ok_or("not signed in with Pubky")?;
    let dir = format!("/pub/music/v1/posts/{id}/");
    let paths = sync
        .homeserver()
        .public_paths(sync.z32(), &dir)
        .await
        .map_err(|e| format!("your homeserver: {e}"))?;
    for p in paths {
        let p = if p.starts_with('/') {
            p
        } else {
            format!("{dir}{p}")
        };
        if let Err(e) = sync.homeserver().delete(sync.session(), &p).await {
            if !e.to_string().contains("404") {
                return Err(format!("your homeserver: {e}"));
            }
        }
    }
    Ok(())
}

// ---- joining (no token) ----

#[derive(Debug, Deserialize)]
pub struct Join {
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Joined {
    pub token: String,
    pub party: String,
    pub expires_at: i64,
}

/// `POST /v1/party/join {code, name?}`: a guest's token for the party behind a link.
pub(crate) async fn join(State(s): State<Arc<NodeState>>, Json(b): Json<Join>) -> ApiResult<Joined> {
    let _g = LOCK.lock().expect("party lock");
    let mut all = load(&s);
    let Some(p) = all.iter_mut().find(|p| p.code == b.code.trim()) else {
        return Err(ApiError::new(
            StatusCode::GONE,
            "This party has ended, or the link is wrong",
        ));
    };
    if p.guests.len() >= MAX_GUESTS {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "This party is full"));
    }
    let name = b
        .name
        .map(|n| n.trim().chars().take(30).collect::<String>())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Guest {}", p.guests.len() + 1));
    let token = format!("{GUEST_PREFIX}{}", crate::tidal::random_urlsafe(32));
    p.guests.push(Guest {
        token_hash: hash(&token),
        name,
        joined_at: now(),
        adds: Vec::new(),
        items: Vec::new(),
    });
    let out = Joined {
        token,
        party: p.name.clone(),
        expires_at: p.expires_at,
    };
    store(&s, &all)?;
    Ok(Json(out))
}

// ---- the guest ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    pub track_id: Option<i64>,
    pub title: String,
    pub artist: String,
    pub cover: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waiting {
    #[serde(flatten)]
    pub song: Song,
    /// Who asked for it.
    pub by: String,
    pub mine: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestView {
    pub party: String,
    pub zone_name: Option<String>,
    pub expires_at: i64,
    pub you: String,
    pub playing: Option<Song>,
    /// Guests' songs still to come, in the order they will play.
    pub waiting: Vec<Waiting>,
    /// Songs you may still add this hour, and have waiting at once.
    pub adds_left: u32,
    pub waiting_left: u32,
    /// When you may add again, if the hourly limit is reached.
    pub next_add_at: Option<i64>,
}

fn cover_of(s: &NodeState, uri: &str) -> Option<String> {
    let tid = uri
        .strip_prefix(crate::sources::TRACK_SCHEME)?
        .parse::<i64>()
        .ok()?;
    let album = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(tid))
        .ok()??
        .album?
        .id?;
    Some(format!("/v1/library/albums/{album}/cover"))
}

fn song_of(s: &NodeState, it: &pm_zones::QueueItem) -> Song {
    Song {
        track_id: it
            .uri
            .strip_prefix(crate::sources::TRACK_SCHEME)
            .and_then(|t| t.parse().ok()),
        title: it.title.clone().unwrap_or_else(|| "Unknown song".into()),
        artist: it.artist.clone().unwrap_or_default(),
        cover: cover_of(s, &it.uri).or_else(|| it.image.clone()),
        duration_ms: it.duration_ms,
    }
}

/// Queue item ids of guests' songs that haven't started yet, by position.
fn waiting_ids(st: &pm_zones::ZoneState) -> impl Fn(u64) -> Option<usize> + '_ {
    move |id| {
        let cur = st.queue_index?;
        st.queue.iter().position(|q| q.id == id).filter(|&p| p > cur)
    }
}

fn guest_view(s: &NodeState, p: &Party, me: usize) -> GuestView {
    let t = now();
    let zone = s.zones.get(&ZoneId::new(&p.zone));
    let st = zone.as_ref().map(|z| z.state());
    let mut waiting = Vec::new();
    let mut mine_waiting = 0u32;
    let mut playing = None;
    if let Some(st) = &st {
        playing = st
            .queue_index
            .and_then(|i| st.queue.get(i))
            .filter(|_| st.transport != Transport::Stopped)
            .map(|it| song_of(s, it));
        let pos = waiting_ids(st);
        let mut found: Vec<(usize, usize, u64)> = Vec::new();
        for (gi, g) in p.guests.iter().enumerate() {
            for &id in &g.items {
                if let Some(at) = pos(id) {
                    found.push((at, gi, id));
                }
            }
        }
        found.sort_unstable();
        for (at, gi, _) in found {
            if gi == me {
                mine_waiting += 1;
            }
            waiting.push(Waiting {
                song: song_of(s, &st.queue[at]),
                by: p.guests[gi].name.clone(),
                mine: gi == me,
            });
        }
    }
    let g = &p.guests[me];
    let recent: Vec<i64> = g.adds.iter().copied().filter(|&a| a > t - HOUR).collect();
    let adds_left = p.adds_per_hour.saturating_sub(recent.len() as u32);
    GuestView {
        party: p.name.clone(),
        zone_name: st.map(|s| s.name),
        expires_at: p.expires_at,
        you: g.name.clone(),
        playing,
        waiting,
        adds_left,
        waiting_left: p.waiting_per_guest.saturating_sub(mine_waiting),
        next_add_at: (adds_left == 0)
            .then(|| recent.iter().min().map(|a| a + HOUR))
            .flatten(),
    }
}

fn me(s: &NodeState, headers: &HeaderMap) -> Result<(Party, usize), ApiError> {
    bearer(headers)
        .and_then(|t| guest_of(s, &t))
        .ok_or_else(|| ApiError::new(StatusCode::GONE, "This party has ended"))
}

/// `GET /v1/party/guest`: what plays, what guests asked for, and what you may still add.
pub(crate) async fn guest(State(s): State<Arc<NodeState>>, headers: HeaderMap) -> ApiResult<GuestView> {
    let (p, i) = me(&s, &headers)?;
    Ok(Json(guest_view(&s, &p, i)))
}

#[derive(Debug, Deserialize)]
pub struct SearchQ {
    pub q: String,
}

/// `GET /v1/party/guest/search?q=`: songs in the library.
pub(crate) async fn search(
    State(s): State<Arc<NodeState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQ>,
) -> ApiResult<Vec<Song>> {
    me(&s, &headers)?;
    let tracks = s
        .library
        .lock()
        .expect("library poisoned")
        .search_tracks(q.q.trim(), 30)?;
    Ok(Json(
        tracks
            .into_iter()
            .filter_map(|t| {
                Some(Song {
                    track_id: Some(t.id?.0),
                    artist: t
                        .artists
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    cover: t
                        .album
                        .as_ref()
                        .and_then(|a| a.id)
                        .map(|id| format!("/v1/library/albums/{id}/cover")),
                    duration_ms: t.duration_ms,
                    title: t.title,
                })
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct Add {
    pub track_id: i64,
}

/// `POST /v1/party/guest/queue {track_id}`: add a song to the guest section, within the limits.
pub(crate) async fn add(
    State(s): State<Arc<NodeState>>,
    headers: HeaderMap,
    Json(b): Json<Add>,
) -> ApiResult<GuestView> {
    let token = bearer(&headers).unwrap_or_default();
    let item = {
        let lib = s.library.lock().expect("library poisoned");
        let track = lib
            .get_track(TrackId(b.track_id))?
            .ok_or_else(|| ApiError::not_found("That song isn't in the library"))?;
        crate::sources::NodeSources::item_for(&track)
            .ok_or_else(|| ApiError::bad_request("That song can't be played"))?
    };
    let h = hash(&token);
    let (zone, play) = {
        let _g = LOCK.lock().expect("party lock");
        let mut all = load(&s);
        let (pi, gi) = all
            .iter()
            .enumerate()
            .find_map(|(pi, p)| Some((pi, p.guests.iter().position(|g| g.token_hash == h)?)))
            .ok_or_else(|| ApiError::new(StatusCode::GONE, "This party has ended"))?;
        let p = &all[pi];
        let zone = s
            .zones
            .get(&ZoneId::new(&p.zone))
            .ok_or_else(|| ApiError::new(StatusCode::GONE, "The party's zone is gone"))?;
        let st = zone.state();
        let t = now();
        let g = &p.guests[gi];
        let recent: Vec<i64> = g.adds.iter().copied().filter(|&a| a > t - HOUR).collect();
        if recent.len() as u32 >= p.adds_per_hour {
            let mins = (recent.iter().min().copied().unwrap_or(t) + HOUR - t + 59) / 60;
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "You've added {} songs this hour; you can add another in {mins} min",
                    p.adds_per_hour
                ),
            ));
        }
        let pos = waiting_ids(&st);
        let mine = g.items.iter().filter(|&&id| pos(id).is_some()).count() as u32;
        if mine >= p.waiting_per_guest {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("You have {mine} songs waiting; add more once one has played"),
            ));
        }
        // After the last guest song still waiting, else right after the current song
        let last_guest = p
            .guests
            .iter()
            .flat_map(|g| g.items.iter())
            .filter_map(|&id| pos(id))
            .max();
        let idle = st.transport == Transport::Stopped || st.queue_index.is_none();
        let ids = if idle {
            zone.enqueue(vec![item], Insert::End)
        } else {
            zone.enqueue(vec![item], Insert::Next)
        };
        let id = ids[0];
        if let (false, Some(l)) = (idle, last_guest) {
            zone.move_item(id, l + 1);
        }
        let p = &mut all[pi];
        let g = &mut p.guests[gi];
        g.adds = recent;
        g.adds.push(t);
        g.items.push(id);
        // Forget items that have played or left the queue
        let live: std::collections::HashSet<u64> = st.queue.iter().map(|q| q.id).collect();
        g.items.retain(|i| *i == id || live.contains(i));
        store(&s, &all)?;
        (zone, idle.then_some(id))
    };
    if let Some(id) = play {
        if let Some(i) = zone.queue().iter().position(|q| q.id == id) {
            zone.play_index(i);
        }
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), zone.wait_acked()).await;
    let (p, i) =
        guest_of(&s, &token).ok_or_else(|| ApiError::new(StatusCode::GONE, "This party has ended"))?;
    Ok(Json(guest_view(&s, &p, i)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn guests_reach_only_the_party() {
        for ok in [
            "/v1/party/guest",
            "/party/guest/search",
            "/v1/library/albums/12/cover",
        ] {
            assert_eq!(guest_refusal(&Method::GET, ok), None, "{ok}");
        }
        assert_eq!(guest_refusal(&Method::POST, "/v1/party/guest/queue"), None);
        for no in [
            "/v1/party",
            "/v1/zones",
            "/v1/library/tracks",
            "/v1/library/albums/12",
            "/v1/library/albums/x/cover",
            "/v1/party/guests",
            "/v1/settings",
        ] {
            assert!(guest_refusal(&Method::GET, no).is_some(), "{no}");
        }
        assert!(guest_refusal(&Method::DELETE, "/v1/library/albums/12/cover").is_some());
        assert!(guest_refusal(&Method::POST, "/v1/zones/z/next").is_some());
    }
}
