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
/// How long whether someone follows someone stays known.
const FOLLOWS_FOR: Duration = Duration::from_secs(600);

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

/// What the node keeps (in memory) about who listens where.
#[derive(Default)]
pub struct Presence {
    /// Zone id → who listens there.
    players: Mutex<HashMap<String, Player>>,
    /// The listen-along groups now.
    along: Mutex<Vec<Along>>,
    /// (person, viewer) → whether the person follows the viewer, and since when that's known.
    follows: Mutex<HashMap<(String, String), (bool, Instant)>>,
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
    host_zone: String,
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
    if let Some((on, at)) = s.presence.follows.lock().expect("presence lock").get(&pair) {
        if at.elapsed() < FOLLOWS_FOR {
            return *on;
        }
    }
    let Some(d) = s.discovery() else { return false };
    let on = match tokio::time::timeout(Duration::from_secs(5), d.nexus().user(viewer, Some(person))).await {
        Ok(Ok(v)) => v.relationship.is_some_and(|r| r.following),
        _ => return false,
    };
    s.presence
        .follows
        .lock()
        .expect("presence lock")
        .insert(pair, (on, Instant::now()));
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
    /// It is the one asking.
    pub you: bool,
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
        if let Some(a) = along.iter().find(|a| a.group == st.id.0) {
            for j in &a.joiners {
                if visible_to(&s, &j.key, &me).await {
                    with.push(j.key.clone());
                }
            }
        }
        out.push(Listening {
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

/// Move what `from` plays to `to`, where it was (stopped and emptied there first).
async fn move_music(from: &Zone, to: &Zone) -> ZoneState {
    let st = from.state();
    from.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), from.wait_stopped()).await;
    from.clear();
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
    let host = zone(s, &a.host_zone)?;
    let mut name = host.name().to_string();
    let mut ids = a.host_outputs.clone();
    for j in &a.joiners {
        if let Some(z) = s.zones.get(&ZoneId::new(&j.zone)) {
            name.push_str(" + ");
            name.push_str(z.name());
        }
        ids.extend(j.outputs.iter().cloned());
    }
    let output = crate::zone_output(s, &name, &ids).await?;
    s.sources.carry_dsp(&host.output().id(), &output.id());
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

#[derive(Debug, Clone, Deserialize)]
pub struct Join {
    /// Whom to listen along with (their key, as `GET /v1/presence` lists it).
    pub key: String,
    /// Your zone, whose speakers join theirs.
    pub zone: String,
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
    if joined(&s, &me) {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "you are listening along already: leave first",
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
    let existing = s
        .presence
        .along
        .lock()
        .expect("presence lock")
        .iter()
        .find(|a| a.group == theirs)
        .cloned();
    if s.presence.sources(&body.zone).is_some() {
        return Err(ApiError::bad_request("that zone is a listen-along group already"));
    }
    let mut along = match existing {
        Some(a) => a,
        None => Along {
            group: String::new(),
            host: body.key.clone(),
            host_zone: theirs.clone(),
            host_outputs: outputs_of(&s, &host_zone).ok_or_else(|| {
                ApiError::bad_request(
                    "their speakers are grouped by their own system; listening along can't join them",
                )
            })?,
            joiners: Vec::new(),
        },
    };
    let outputs = outputs_of(&s, &mine).ok_or_else(|| {
        ApiError::bad_request("your speakers are grouped by their own system; pick one of them")
    })?;
    let taken: Vec<&String> = along
        .host_outputs
        .iter()
        .chain(along.joiners.iter().flat_map(|j| j.outputs.iter()))
        .collect();
    if outputs.iter().any(|o| taken.contains(&o)) {
        return Err(ApiError::bad_request("those speakers play their music already"));
    }
    along.joiners.push(Joiner {
        key: me,
        zone: body.zone.clone(),
        outputs,
    });
    let since = s
        .presence
        .players
        .lock()
        .expect("presence lock")
        .get(&theirs)
        .map(|p| p.since)
        .unwrap_or_else(pm_library::db::now_unix);
    mine.stop();
    let group = build(&s, &along).await?;
    let st = move_music(&host_zone, &group).await;
    if host_zone.id().0 != along.host_zone {
        // It was a group already: the new one takes its place
        drop_group(&s, &host_zone);
    }
    along.group = group.id().0.clone();
    s.presence.players.lock().expect("presence lock").insert(
        along.group.clone(),
        Player {
            key: along.host.clone(),
            since,
        },
    );
    let mut all = s.presence.along.lock().expect("presence lock");
    all.retain(|a| a.group != theirs);
    all.push(along);
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
    let mut along = {
        let all = s.presence.along.lock().expect("presence lock");
        match &q.group {
            Some(g) => {
                let a = all
                    .iter()
                    .find(|a| &a.group == g)
                    .cloned()
                    .ok_or_else(|| ApiError::not_found("no such listen-along group"))?;
                if a.host != me && c.role != Role::Owner {
                    return Err(accounts::refused("only its host or the owner ends it"));
                }
                a
            }
            None => all
                .iter()
                .find(|a| a.joiners.iter().any(|j| j.key == me))
                .cloned()
                .ok_or_else(|| ApiError::not_found("you aren't listening along"))?,
        }
    };
    let group = zone(&s, &along.group)?;
    let old = along.group.clone();
    if q.group.is_some() {
        along.joiners.clear();
    } else {
        along.joiners.retain(|j| j.key != me);
    }
    let (st, now) = if along.joiners.is_empty() {
        // The last one left: back to their own zone
        let home = zone(&s, &along.host_zone)?;
        let st = move_music(&group, &home).await;
        (st, None)
    } else {
        let next = build(&s, &along).await?;
        let st = move_music(&group, &next).await;
        along.group = next.id().0.clone();
        (st, Some(along.clone()))
    };
    let since = s
        .presence
        .players
        .lock()
        .expect("presence lock")
        .get(&old)
        .map(|p| p.since)
        .unwrap_or_else(pm_library::db::now_unix);
    drop_group(&s, &group);
    s.presence.players.lock().expect("presence lock").insert(
        st.id.0.clone(),
        Player {
            key: along.host.clone(),
            since,
        },
    );
    let mut all = s.presence.along.lock().expect("presence lock");
    all.retain(|a| a.group != old);
    all.extend(now);
    Ok(Json(st))
}
