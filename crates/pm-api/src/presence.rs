//! Who on this node is listening to what, and listening along with them (listening together on
//! one node). Nothing is published: it lives on this node, in memory.
//!
//! - **Who listens where:** the last person who queued, played, paused or moved music on a zone
//!   (the owner or a listener) is the one listening there. A restart forgets it.
//! - **Each person chooses who sees it** (`PUT /v1/me/presence {visible}`): `everyone` on this
//!   node, `followed` (the people they follow on Pubky, as Nexus knows it), or `hidden`, the
//!   default. Nobody else sees a hidden person, the owner included.
//! - **Listening along** (`POST /v1/presence/along {key, zone}`): your zone's speakers join theirs
//!   as one synced group (the same groups as `POST /v1/zones` with `outputs`), so both hear one
//!   stream at the same quality, through their zone's DSP. Their music moves into the group where
//!   it was. Leaving (`DELETE /v1/presence/along`) takes your speakers out again; when the last
//!   one leaves, the music moves back to their own zone. They (or the owner) can end it for all.

use crate::accounts::{self, Caller, Role};
use crate::{ApiError, ApiResult, NodeState};
use axum::extract::{Query, State};
use axum::{Extension, Json};
use pm_zones::{OutputId, Transport, Zone, ZoneId, ZoneState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Who an owner is when they have not signed in to Pubky (no key to name them by).
pub const OWNER: &str = "owner";
const VISIBLE_KEY: &str = "presence.visible.";

/// Where a person's "who may see me listening" choice is kept.
pub(crate) fn visible_key(key: &str) -> String {
    format!("{VISIBLE_KEY}{key}")
}
/// How long whether someone follows someone stays known...
const FOLLOWS_FOR: Duration = Duration::from_secs(600);
/// ...and that Nexus couldn't say (taken as no meanwhile, so a page isn't kept waiting).
const FOLLOWS_UNKNOWN_FOR: Duration = Duration::from_secs(60);

/// Who may see that a person is listening.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visible {
    /// Everyone signed in to this node.
    Everyone,
    /// The people they follow on Pubky.
    Followed,
    /// Nobody else.
    #[default]
    Hidden,
}

/// Whether someone follows someone: the answer, when it came, and how long it holds.
type Known = (bool, Instant, Duration);

/// What the node keeps (in memory) about who listens where.
#[derive(Default)]
pub struct Presence {
    /// Zone id → who listens there.
    players: Mutex<HashMap<String, Player>>,
    /// The listen-along groups now.
    along: Mutex<Vec<Along>>,
    /// (person, viewer) → whether the person follows the viewer, since when that's known, and
    /// for how long.
    follows: Mutex<HashMap<(String, String), Known>>,
    /// One listen-along change at a time (they move music between zones).
    busy: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone)]
struct Player {
    key: String,
    since: i64,
}

/// A listen-along group: the host's zone and outputs, and who joined with which.
#[derive(Debug, Clone)]
struct Along {
    /// The group zone's id.
    group: String,
    host: String,
    /// The host's own zone, where the music goes back: stopped meanwhile, its queue kept (and
    /// saved, so a restart doesn't lose it).
    host_zone: String,
    /// Its name and output, for the group's name and DSP.
    host_name: String,
    host_output: OutputId,
    /// Its queue version when the music left: the same at the end means nobody used it since,
    /// so the music may go back there.
    home_version: u64,
    host_outputs: Vec<String>,
    joiners: Vec<Joiner>,
}

#[derive(Debug, Clone)]
struct Joiner {
    key: String,
    zone: String,
    outputs: Vec<String>,
}

impl Presence {
    /// The zones a listen-along group plays for (its host's and its joiners'), so a listener may
    /// use the group when they may use all of those.
    pub fn sources(&self, zone: &str) -> Option<Vec<String>> {
        let along = self.along.lock().expect("presence lock");
        along.iter().find(|a| a.group == zone).map(|a| {
            std::iter::once(a.host_zone.clone())
                .chain(a.joiners.iter().map(|j| j.zone.clone()))
                .collect()
        })
    }

    /// Whether `output` plays in a listen-along group (as the host's speaker or a joiner's).
    fn in_a_group(&self, output: &str) -> bool {
        let along = self.along.lock().expect("presence lock");
        along.iter().any(|a| {
            a.host_outputs.iter().any(|o| o == output)
                || a.joiners.iter().any(|j| j.outputs.iter().any(|o| o == output))
        })
    }
}

/// Who this caller is for presence: their Pubky key, [`OWNER`] for an owner without one; a
/// party guest is nobody.
pub fn who(c: &Caller) -> Option<String> {
    match c.role {
        Role::Guest => None,
        Role::Owner => Some(c.key.clone().unwrap_or_else(|| OWNER.into())),
        Role::Listener => c.key.clone(),
    }
}

/// The zone a request plays on, when it is one that makes the caller the zone's listener
/// (queueing, transport, loading, radio); `None` for anything else.
pub fn zone_written(method: &axum::http::Method, path: &str) -> Option<String> {
    if *method == axum::http::Method::GET || *method == axum::http::Method::HEAD {
        return None;
    }
    let path = path.strip_prefix("/v1").unwrap_or(path);
    let (zone, what) = path.strip_prefix("/zones/")?.split_once('/')?;
    let what = what.split('/').next().unwrap_or(what);
    matches!(what, "queue" | "transport" | "load" | "radio" | "shuffle").then(|| zone.to_string())
}

/// `c` plays on `zone` now. A listen-along group stays its host's, whoever pauses it.
pub fn played(s: &NodeState, zone: &str, c: &Caller) {
    let Some(key) = who(c) else { return };
    if s.presence.sources(zone).is_some() {
        return;
    }
    let mut players = s.presence.players.lock().expect("presence lock");
    match players.get_mut(zone) {
        Some(p) if p.key == key => {}
        _ => {
            players.insert(
                zone.to_string(),
                Player {
                    key,
                    since: pm_library::db::now_unix(),
                },
            );
        }
    }
}

fn visible_setting(s: &NodeState, key: &str) -> Visible {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(&format!("{VISIBLE_KEY}{key}"))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(serde_json::Value::String(v)).ok())
        .unwrap_or_default()
}

/// Whether `person` follows `viewer` on Pubky (Nexus, remembered a while). `false` when it can't
/// be known: nobody is shown by mistake.
async fn follows(s: &NodeState, person: &str, viewer: &str) -> bool {
    if person == OWNER || viewer == OWNER {
        return false;
    }
    let pair = (person.to_string(), viewer.to_string());
    if let Some((on, at, ttl)) = s.presence.follows.lock().expect("presence lock").get(&pair) {
        if at.elapsed() < *ttl {
            return *on;
        }
    }
    let Some(d) = s.discovery() else { return false };
    let (on, ttl) =
        match tokio::time::timeout(Duration::from_secs(5), d.nexus().user(viewer, Some(person))).await {
            Ok(Ok(v)) => (v.relationship.is_some_and(|r| r.following), FOLLOWS_FOR),
            _ => (false, FOLLOWS_UNKNOWN_FOR),
        };
    s.presence
        .follows
        .lock()
        .expect("presence lock")
        .insert(pair, (on, Instant::now(), ttl));
    on
}

/// Whether `viewer` may see that `person` is listening.
async fn visible_to(s: &NodeState, person: &str, viewer: &str) -> bool {
    if person == viewer {
        return true;
    }
    match visible_setting(s, person) {
        Visible::Everyone => true,
        Visible::Followed => follows(s, person, viewer).await,
        Visible::Hidden => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceSetting {
    pub visible: Visible,
}

/// `GET /v1/me/presence`: who may see what you listen to.
pub async fn setting_get(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
) -> ApiResult<PresenceSetting> {
    let key = who(&c).ok_or_else(|| accounts::refused("that is for people on this node"))?;
    Ok(Json(PresenceSetting {
        visible: visible_setting(&s, &key),
    }))
}

/// `PUT /v1/me/presence {visible}`: `everyone`, `followed` or `hidden`.
pub async fn setting_put(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Json(body): Json<PresenceSetting>,
) -> ApiResult<PresenceSetting> {
    let key = who(&c).ok_or_else(|| accounts::refused("that is for people on this node"))?;
    let value = serde_json::to_value(body.visible).expect("serializes");
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(&format!("{VISIBLE_KEY}{key}"), value.as_str().unwrap_or("hidden"))?;
    Ok(Json(body))
}

/// A zone, by id and name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneRef {
    pub id: String,
    pub name: String,
}

/// Someone listening on this node, as `GET /v1/presence` shows them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listening {
    /// Their Pubky key ([`OWNER`] for an owner not signed in to Pubky).
    pub key: String,
    /// Their name and picture (`/v1/profiles/<key>/avatar`), when their profile is known.
    pub name: Option<String>,
    pub avatar: Option<String>,
    /// It is the one asking.
    pub you: bool,
    /// The one asking listens along with them.
    pub mine: bool,
    /// It is the one asking, and others listen along: their music plays on the group.
    pub hosting: bool,
    /// Either way: the one asking's own zone, where they go back to when it ends.
    pub your_zone: Option<String>,
    /// Playing, or paused.
    pub playing: bool,
    pub uri: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub image: Option<String>,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    /// When they started listening on this zone (unix seconds).
    pub since: i64,
    /// The zone, when the one asking may play on it (else they only see the music).
    pub zone: Option<ZoneRef>,
    /// The one asking can listen along: they may play on the zone, it isn't theirs, and they
    /// aren't listening along already.
    pub can_join: bool,
    /// Who listens along with them (those the one asking may see).
    pub along: Vec<String>,
}

/// The zones playing now, with who listens there, newest first; pruned of groups that are gone.
fn now_playing(s: &NodeState) -> Vec<(ZoneState, Player)> {
    s.presence
        .along
        .lock()
        .expect("presence lock")
        .retain(|a| s.zones.get(&ZoneId::new(&a.group)).is_some());
    let players = s.presence.players.lock().expect("presence lock").clone();
    let mut out: Vec<(ZoneState, Player)> = players
        .into_iter()
        .filter_map(|(zone, p)| {
            let st = s.zones.get(&ZoneId::new(&zone))?.state();
            (st.transport != Transport::Stopped && st.now_playing.is_some()).then_some((st, p))
        })
        .collect();
    out.sort_by(|a, b| b.1.since.cmp(&a.1.since).then_with(|| a.0.id.0.cmp(&b.0.id.0)));
    out
}

fn joined(s: &NodeState, key: &str) -> bool {
    s.presence
        .along
        .lock()
        .expect("presence lock")
        .iter()
        .any(|a| a.joiners.iter().any(|j| j.key == key))
}

/// `GET /v1/presence`: who on this node is listening to what, as each of them lets the one
/// asking see.
pub async fn list(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
) -> ApiResult<Vec<Listening>> {
    let me = who(&c).ok_or_else(|| accounts::refused("that is for people on this node"))?;
    let along = s.presence.along.lock().expect("presence lock").clone();
    let along_me = joined(&s, &me);
    let mut out = Vec::new();
    for (st, p) in now_playing(&s) {
        if !visible_to(&s, &p.key, &me).await {
            continue;
        }
        let Some(np) = st.now_playing.as_ref() else {
            continue;
        };
        let may = accounts::may_use_zone(&s, &c, &st.id.0);
        let mut with = Vec::new();
        let group = along.iter().find(|a| a.group == st.id.0);
        let joined_from = group
            .and_then(|a| a.joiners.iter().find(|j| j.key == me))
            .map(|j| j.zone.clone());
        let mine = joined_from.is_some();
        let hosting = group.is_some_and(|a| a.host == me);
        let your_zone = joined_from.or_else(|| group.filter(|_| hosting).map(|a| a.host_zone.clone()));
        if let Some(a) = group {
            for j in &a.joiners {
                if visible_to(&s, &j.key, &me).await {
                    with.push(j.key.clone());
                }
            }
        }
        out.push(Listening {
            name: None,
            avatar: None,
            mine,
            hosting,
            your_zone,
            you: p.key == me,
            can_join: may && p.key != me && !along_me,
            key: p.key,
            playing: st.transport == Transport::Playing,
            uri: np.item.uri.clone(),
            title: np.item.title.clone(),
            artist: np.item.artist.clone(),
            album: np.item.album.clone(),
            image: np.item.image.clone(),
            position_ms: np.position_ms,
            duration_ms: np.duration_ms.or(np.item.duration_ms),
            since: p.since,
            zone: may.then(|| ZoneRef {
                id: st.id.0.clone(),
                name: st.name.clone(),
            }),
            along: with,
        });
    }
    let keys: Vec<String> = out.iter().map(|l| l.key.clone()).collect();
    let cards = crate::profiles::known_cards(&s, &keys).await;
    for l in &mut out {
        if let Some(c) = cards.get(&l.key) {
            l.name = Some(c.name.clone());
            l.avatar = c.avatar.clone();
        }
    }
    Ok(Json(out))
}

/// The registry outputs a zone plays on: its one output, or the members of a group the node
/// keeps in sync; `None` for a group a speaker system keeps (Sonos, BluOS, HEOS).
fn outputs_of(s: &NodeState, zone: &Zone) -> Option<Vec<String>> {
    let id = zone.output().id();
    if s.outputs.get(&id).is_some() {
        return Some(vec![id.0]);
    }
    let members: Vec<String> =
        id.0.strip_prefix("group:")?
            .split('+')
            .map(str::to_string)
            .collect();
    members
        .iter()
        .all(|m| s.outputs.get(&OutputId(m.clone())).is_some())
        .then_some(members)
}

fn zone(s: &NodeState, id: &str) -> Result<Arc<Zone>, ApiError> {
    s.zones
        .get(&ZoneId::new(id))
        .ok_or_else(|| ApiError::not_found(format!("no zone {id}")))
}

/// Move what `from` plays to `to`, where it was (`to` is stopped and emptied first). `from`
/// stops; it is emptied too unless it keeps its queue (the host's own zone, while a group
/// plays its music).
async fn move_music(from: &Zone, to: &Zone, empty_from: bool) -> ZoneState {
    let st = from.state();
    from.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), from.wait_stopped()).await;
    if empty_from {
        from.clear();
    }
    let index = st.now_playing.as_ref().map(|n| n.index).or(st.queue_index);
    let position_ms = st.now_playing.as_ref().map(|n| n.position_ms).unwrap_or(0);
    crate::load_zone(
        to,
        st.queue,
        index,
        position_ms,
        st.transport,
        st.repeat,
        st.radio,
    )
    .await
}

/// The group zone for `a`: the host's speakers first (the leader), then each joiner's, through
/// the host zone's DSP.
async fn build(s: &Arc<NodeState>, a: &Along) -> Result<Arc<Zone>, ApiError> {
    let mut name = a.host_name.clone();
    let mut ids = a.host_outputs.clone();
    for j in &a.joiners {
        if let Some(z) = s.zones.get(&ZoneId::new(&j.zone)) {
            name.push_str(" + ");
            name.push_str(z.name());
        }
        ids.extend(j.outputs.iter().cloned());
    }
    let output = crate::zone_output(s, &name, &ids).await?;
    s.sources.carry_dsp(&a.host_output, &output.id());
    Ok(s.create_zone(&name, output))
}

/// Take a group zone away (its music already moved).
fn drop_group(s: &NodeState, group: &Zone) {
    s.sources.forget_dsp(&group.output().id());
    s.remove_zone(group.id());
    s.presence
        .players
        .lock()
        .expect("presence lock")
        .remove(&group.id().0);
}

/// Record `host` as the one listening on `zone`, since `since`.
fn host_plays(s: &NodeState, zone: &str, host: &str, since: i64) {
    s.presence.players.lock().expect("presence lock").insert(
        zone.to_string(),
        Player {
            key: host.to_string(),
            since,
        },
    );
}

fn since_on(s: &NodeState, zone: &str) -> i64 {
    s.presence
        .players
        .lock()
        .expect("presence lock")
        .get(zone)
        .map(|p| p.since)
        .unwrap_or_else(pm_library::db::now_unix)
}

/// End group `a` (its record already taken out): the music goes back to the host's own zone
/// when that is still as the music left it (stopped, its queue untouched); when the host has
/// played something else there meanwhile, the group just stops. When their zone is gone, the
/// music stays where it is, in a zone of its own. Answers the zone the music is in now.
async fn end_group(s: &Arc<NodeState>, a: &Along) -> Result<ZoneState, ApiError> {
    let group = zone(s, &a.group)?;
    let since = since_on(s, &a.group);
    let Some(home) = s.zones.get(&ZoneId::new(&a.host_zone)) else {
        // Nowhere to go back to: the group plays on, as an ordinary zone
        return Ok(group.state());
    };
    let untouched = {
        let h = home.state();
        h.transport == Transport::Stopped && h.queue_version == a.home_version
    };
    let st = if untouched {
        move_music(&group, &home, true).await
    } else {
        group.stop();
        let _ = tokio::time::timeout(Duration::from_secs(5), group.wait_stopped()).await;
        home.state()
    };
    drop_group(s, &group);
    if untouched {
        host_plays(s, &a.host_zone, &a.host, since);
    }
    Ok(st)
}

fn take_along(s: &NodeState, group: &str) -> Option<Along> {
    let mut all = s.presence.along.lock().expect("presence lock");
    let i = all.iter().position(|a| a.group == group)?;
    Some(all.remove(i))
}

/// `DELETE /v1/zones/{id}` for a listen-along group: it ends as when its host ends it, so the
/// music isn't lost. `false` when `zone` isn't one.
pub async fn delete_group(s: &Arc<NodeState>, zone_id: &str) -> Result<bool, ApiError> {
    let _busy = s.presence.busy.lock().await;
    let Some(a) = take_along(s, zone_id) else {
        return Ok(false);
    };
    end_group(s, &a).await?;
    // Ended with nowhere to go back to: delete it as asked
    if let Some(g) = s.zones.get(&ZoneId::new(zone_id)) {
        g.stop();
        drop_group(s, &g);
    }
    Ok(true)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Join {
    /// Whom to listen along with (their key, as `GET /v1/presence` lists it).
    pub key: String,
    /// Your zone, whose speakers join theirs.
    pub zone: String,
}

fn conflict(why: &str) -> ApiError {
    ApiError::new(axum::http::StatusCode::CONFLICT, why)
}

/// `POST /v1/presence/along {key, zone}`: listen along with someone on this node. Answers the
/// group zone, now playing their music on both.
pub async fn join(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Json(body): Json<Join>,
) -> ApiResult<ZoneState> {
    let me = who(&c).ok_or_else(|| accounts::refused("that is for people on this node"))?;
    if body.key == me {
        return Err(ApiError::bad_request("that is you"));
    }
    let _busy = s.presence.busy.lock().await;
    now_playing(&s);
    if joined(&s, &me) {
        return Err(conflict("you are listening along already: leave first"));
    }
    if s.presence
        .along
        .lock()
        .expect("presence lock")
        .iter()
        .any(|a| a.host == me)
    {
        return Err(conflict(
            "others listen along with you: end that first, or leave it to them",
        ));
    }
    // Their zone: where they listen now, if they let you see it
    let mut theirs = None;
    for (st, p) in now_playing(&s) {
        if p.key == body.key && visible_to(&s, &p.key, &me).await {
            theirs = Some(st.id.0);
            break;
        }
    }
    let theirs = theirs.ok_or_else(|| ApiError::not_found("they aren't listening now"))?;
    if !accounts::may_use_zone(&s, &c, &theirs) || !accounts::may_use_zone(&s, &c, &body.zone) {
        return Err(accounts::refused(
            "the owner has not opened that zone to listeners",
        ));
    }
    let host_zone = zone(&s, &theirs)?;
    let mine = zone(&s, &body.zone)?;
    if theirs == body.zone {
        return Err(ApiError::bad_request("that is the zone they listen on"));
    }
    if s.presence.sources(&body.zone).is_some() {
        return Err(ApiError::bad_request("that zone is a listen-along group already"));
    }
    // Your zone stops to join: never one where someone else's music plays (whether or not they
    // let you see them)
    let busy = {
        let st = mine.state();
        st.transport != Transport::Stopped && st.now_playing.is_some()
    };
    let someone_else = s
        .presence
        .players
        .lock()
        .expect("presence lock")
        .get(&body.zone)
        .is_some_and(|p| p.key != me);
    if busy && someone_else {
        return Err(conflict(
            "someone else is listening on that zone: listen along from yours",
        ));
    }
    let outputs = outputs_of(&s, &mine).ok_or_else(|| {
        ApiError::bad_request(
            "the zone you picked is a speaker group its maker keeps (Sonos, BluOS, HEOS): pick the zone of one of its speakers",
        )
    })?;
    // A speaker plays in one group at a time (theirs, or anyone else's)
    if outputs.iter().any(|o| s.presence.in_a_group(o)) {
        return Err(conflict("those speakers play in a listen-along group already"));
    }
    let existing = take_along(&s, &theirs);
    let mut along = match existing.clone() {
        Some(a) => a,
        None => Along {
            group: String::new(),
            host: body.key.clone(),
            host_zone: theirs.clone(),
            host_name: host_zone.name().to_string(),
            host_output: host_zone.output().id(),
            home_version: 0,
            host_outputs: outputs_of(&s, &host_zone).ok_or_else(|| {
                ApiError::bad_request(
                    "they listen on a speaker group its maker keeps (Sonos, BluOS, HEOS): listening along can't join it",
                )
            })?,
            joiners: Vec::new(),
        },
    };
    // A speaker plays in one group at a time: none of yours among theirs, and on a first join none
    // of theirs in anyone else's group
    let clash = outputs.iter().any(|o| along.host_outputs.contains(o))
        || (existing.is_none() && along.host_outputs.iter().any(|o| s.presence.in_a_group(o)));
    if clash {
        s.presence.along.lock().expect("presence lock").extend(existing);
        return Err(conflict("those speakers play in a listen-along group already"));
    }
    along.joiners.push(Joiner {
        key: me,
        zone: body.zone.clone(),
        outputs,
    });
    let since = since_on(&s, &theirs);
    let group = match build(&s, &along).await {
        Ok(g) => g,
        Err(e) => {
            // Nothing changed: their group (if any) stays as it was
            s.presence.along.lock().expect("presence lock").extend(existing);
            return Err(e);
        }
    };
    // Your speakers are free before the group opens them
    mine.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), mine.wait_stopped()).await;
    let regroup = existing.is_some();
    let st = move_music(&host_zone, &group, regroup).await;
    if !regroup {
        along.home_version = host_zone.state().queue_version;
    }
    along.group = group.id().0.clone();
    host_plays(&s, &along.group, &along.host, since);
    // The record first: a page that sees the old group go finds the new one
    s.presence.along.lock().expect("presence lock").push(along);
    if regroup {
        drop_group(&s, &host_zone);
    }
    Ok(Json(st))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Leave {
    /// A group to end for everyone (its host or the owner); without it, you leave the one you
    /// joined.
    #[serde(default)]
    pub group: Option<String>,
}

/// `DELETE /v1/presence/along[?group=]`: stop listening along (your speakers leave the group),
/// or end a group for everyone (its host, or the owner). Answers the zone the music plays on
/// now.
pub async fn leave(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Query(q): Query<Leave>,
) -> ApiResult<ZoneState> {
    let me = who(&c).ok_or_else(|| accounts::refused("that is for people on this node"))?;
    let _busy = s.presence.busy.lock().await;
    now_playing(&s);
    let found = {
        let all = s.presence.along.lock().expect("presence lock");
        match &q.group {
            Some(g) => {
                let a = all
                    .iter()
                    .find(|a| &a.group == g)
                    .ok_or_else(|| ApiError::not_found("no such listen-along group"))?;
                if a.host != me && c.role != Role::Owner {
                    return Err(accounts::refused("only its host or the owner ends it"));
                }
                a.group.clone()
            }
            None => all
                .iter()
                .find(|a| a.joiners.iter().any(|j| j.key == me))
                .map(|a| a.group.clone())
                .ok_or_else(|| ApiError::not_found("you aren't listening along"))?,
        }
    };
    let mut along =
        take_along(&s, &found).ok_or_else(|| ApiError::not_found("no such listen-along group"))?;
    if q.group.is_some() {
        along.joiners.clear();
    } else {
        along.joiners.retain(|j| j.key != me);
    }
    if along.joiners.is_empty() {
        return Ok(Json(end_group(&s, &along).await?));
    }
    let group = zone(&s, &along.group)?;
    let since = since_on(&s, &along.group);
    let next = match build(&s, &along).await {
        Ok(n) => n,
        Err(e) => {
            // Leaving can't fail on a rebuild: end it for all instead
            tracing::warn!(error = ?e, "listening along: rebuilding the group failed, ending it");
            return Ok(Json(end_group(&s, &along).await?));
        }
    };
    let st = move_music(&group, &next, true).await;
    along.group = next.id().0.clone();
    host_plays(&s, &along.group, &along.host, since);
    s.presence.along.lock().expect("presence lock").push(along);
    drop_group(&s, &group);
    Ok(Json(st))
}
