//! Pubky profiles (STEWARD-UX-PEOPLE.md; not the node's own users, which are `people.rs`): the faces of who tagged something, and a page per
//! person. A profile is the person's own `/pub/pubky.app/profile.json`, read through Nexus (or
//! straight from their homeserver when Nexus doesn't know them yet). The node never writes a
//! profile of its own; following writes the standard `/pub/pubky.app/follows/<id>` object that
//! the person's other Pubky apps already use.

use crate::search::SearchHit;
use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use pm_pubky::nexus::{NexusError, UserView};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Profiles are kept an hour, avatars a day.
const PROFILE_TTL: Duration = Duration::from_secs(3600);
const AVATAR_TTL: Duration = Duration::from_secs(86_400);

/// A person on a tag chip or in a list.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PersonCard {
    /// Raw z32 key.
    pub id: String,
    /// Their name, or a short form of the key when Nexus doesn't know them yet.
    pub name: String,
    /// `/v1/profiles/<id>/avatar`, when they have a picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// The person signed in on this node.
    #[serde(default)]
    pub you: bool,
    /// You follow them.
    #[serde(default)]
    pub following: bool,
    /// Nexus has indexed them.
    #[serde(default)]
    pub known: bool,
}

/// `GET /v1/profiles/<id>`: the profile page.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersonPage {
    #[serde(flatten)]
    pub card: PersonCard,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub links: Vec<pm_pubky::nexus::UserLink>,
    #[serde(default)]
    pub followed_by: bool,
    #[serde(default)]
    pub followers: u64,
    #[serde(default)]
    pub following_count: u64,
    /// Their profile on pubky.app (editing a profile stays there).
    pub profile_url: String,
    /// The music they tagged publicly: labels with how often, most used first.
    #[serde(default)]
    pub tags: Vec<(String, i64)>,
    /// What they tagged most recently (Pinakes, `author=<id>`).
    #[serde(default)]
    pub tagged: Vec<SearchHit>,
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 52 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `abcd…wxyz`: a key as people read it.
pub(crate) fn short(id: &str) -> String {
    if id.len() > 10 {
        format!("{}…{}", &id[..4], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

/// Profiles by key, with when they were asked (`None`: Nexus doesn't know them).
type ProfileCache = Mutex<HashMap<String, (Instant, Option<UserView>)>>;

fn profiles() -> &'static ProfileCache {
    static C: OnceLock<ProfileCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn me(s: &NodeState) -> Option<String> {
    s.sync_handle().map(|h| h.z32().to_string())
}

/// Follows made on this node win over Nexus for a while (Nexus indexes them a little later).
const FOLLOW_KEY: &str = "profiles.follow.";

fn local_follow(s: &NodeState, id: &str) -> Option<bool> {
    let v = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(&format!("{FOLLOW_KEY}{id}"))
        .ok()
        .flatten()?;
    let (on, at) = v.split_once(':')?;
    let at: i64 = at.parse().ok()?;
    (pm_library::db::now_unix() - at < 3600).then_some(on == "1")
}

/// Profiles for these people, batched and cached (one Nexus call for the ones not cached).
async fn views(s: &NodeState, ids: &[String]) -> HashMap<String, Option<UserView>> {
    let mut out = HashMap::new();
    let mut missing = Vec::new();
    {
        let cache = profiles().lock().expect("profiles");
        for id in ids {
            match cache.get(id) {
                Some((at, v)) if at.elapsed() < PROFILE_TTL => {
                    out.insert(id.clone(), v.clone());
                }
                _ => missing.push(id.clone()),
            }
        }
    }
    if missing.is_empty() {
        return out;
    }
    let Some(d) = s.discovery() else {
        for id in missing {
            out.insert(id, None);
        }
        return out;
    };
    let viewer = me(s);
    let got = tokio::time::timeout(
        Duration::from_secs(4),
        d.nexus().users_by_ids(&missing, viewer.as_deref()),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .unwrap_or_default();
    let mut cache = profiles().lock().expect("profiles");
    for id in missing {
        let v = got.iter().find(|u| u.details.id == id).cloned();
        cache.insert(id.clone(), (Instant::now(), v.clone()));
        out.insert(id, v);
    }
    out
}

fn card(s: &NodeState, id: &str, v: Option<&UserView>, me: Option<&str>) -> PersonCard {
    PersonCard {
        id: id.to_string(),
        name: v
            .map(|u| u.details.name.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| short(id)),
        avatar: v
            .filter(|u| u.details.image.is_some())
            .map(|_| format!("/v1/profiles/{id}/avatar")),
        you: me == Some(id),
        following: local_follow(s, id)
            .or_else(|| v.and_then(|u| u.relationship.as_ref()).map(|r| r.following))
            .unwrap_or(false),
        known: v.is_some(),
    }
}

#[derive(Debug, Deserialize)]
pub struct PeopleQuery {
    /// Comma-separated z32 keys (up to 50).
    pub ids: String,
}

/// `GET /v1/profiles?ids=a,b`: the people behind tag chips, in the order asked.
pub(crate) async fn list(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<PeopleQuery>,
) -> ApiResult<Vec<PersonCard>> {
    let mut ids: Vec<String> = Vec::new();
    for id in q.ids.split(',').map(str::trim).filter(|x| valid_id(x)) {
        if !ids.iter().any(|x| x == id) {
            ids.push(id.to_string());
        }
    }
    ids.truncate(50);
    let found = views(&s, &ids).await;
    let me = me(&s);
    Ok(Json(
        ids.iter()
            .map(|id| card(&s, id, found.get(id).and_then(|v| v.as_ref()), me.as_deref()))
            .collect(),
    ))
}

/// `GET /v1/profiles/<id>`: their profile and the music they tag in public.
pub(crate) async fn page(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<PersonPage> {
    if !valid_id(&id) {
        return Err(ApiError::bad_request("that isn't a Pubky key"));
    }
    let me = me(&s);
    let d = s.discovery();
    // One profile with the viewer's relationship (fresh: the follow button needs it)
    let view = match &d {
        Some(d) => {
            match tokio::time::timeout(Duration::from_secs(5), d.nexus().user(&id, me.as_deref())).await {
                Ok(Ok(v)) => Some(v),
                _ => None,
            }
        }
        None => None,
    };
    // Not on Nexus yet: their own profile.json, straight from their homeserver
    let view = match view {
        Some(v) => Some(v),
        None => profile_from_homeserver(&s, &id).await,
    };
    if let Some(v) = &view {
        profiles()
            .lock()
            .expect("profiles")
            .insert(id.clone(), (Instant::now(), Some(v.clone())));
    }
    let mut pg = PersonPage {
        card: card(&s, &id, view.as_ref(), me.as_deref()),
        profile_url: format!("https://pubky.app/profile/{id}"),
        ..Default::default()
    };
    if let Some(v) = &view {
        pg.bio = v.details.bio.clone().filter(|b| !b.trim().is_empty());
        pg.status = v.details.status.clone().filter(|b| !b.trim().is_empty());
        pg.links = v
            .details
            .links
            .iter()
            .filter(|l| l.url.starts_with("http"))
            .cloned()
            .collect();
        pg.followed_by = v.relationship.as_ref().is_some_and(|r| r.followed_by);
        pg.followers = v.counts.followers;
        pg.following_count = v.counts.following;
    }
    // Their music: what they tagged, through Pinakes
    if let Some(d) = &d {
        let q = pm_pubky::pinakes::SearchQuery {
            author: Some(id.clone()),
            collapse: Some("group".into()),
            facet: Some("tags".into()),
            sort: Some("recent".into()),
            limit: Some(40),
            ..Default::default()
        };
        if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(5), d.pinakes().search(&q)).await {
            pg.tags = r
                .facets
                .as_ref()
                .map(|f| f.tags.iter().map(|t| (t.label.clone(), t.count)).collect())
                .unwrap_or_default();
            pg.tagged = r.items.iter().filter_map(crate::search::card_hit).collect();
        }
    }
    Ok(Json(pg))
}

/// A person Nexus doesn't know yet: their `/pub/pubky.app/profile.json`, read from their homeserver.
async fn profile_from_homeserver(s: &NodeState, id: &str) -> Option<UserView> {
    let h = s.homeserver().ok()?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        h.get_bytes(
            id,
            "/pub/pubky.app/profile.json",
            pm_pubky::Visibility::Public,
            None,
        ),
    )
    .await
    .ok()?
    .ok()?;
    let p: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(UserView {
        details: pm_pubky::nexus::UserDetails {
            id: id.to_string(),
            name: p["name"].as_str().unwrap_or_default().chars().take(50).collect(),
            bio: p["bio"].as_str().map(|b| b.chars().take(500).collect()),
            status: p["status"].as_str().map(|b| b.chars().take(100).collect()),
            image: None,
            links: p["links"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .take(10)
                        .filter_map(|x| {
                            Some(pm_pubky::nexus::UserLink {
                                title: x["title"].as_str()?.chars().take(50).collect(),
                                url: x["url"].as_str()?.chars().take(300).collect(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        ..Default::default()
    })
}

/// `GET /v1/profiles/<id>/avatar`: their picture, kept on the node a day (browsers never ask Nexus).
pub(crate) async fn avatar(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if !valid_id(&id) {
        return Err(ApiError::bad_request("that isn't a Pubky key"));
    }
    let dir = s.art_dir.join("people");
    let path = dir.join(&id);
    let fresh = tokio::fs::metadata(&path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < AVATAR_TTL);
    if !fresh {
        let d = s.discovery().ok_or_else(|| ApiError::not_found("no picture"))?;
        match tokio::time::timeout(Duration::from_secs(5), d.nexus().avatar(&id)).await {
            Ok(Ok((bytes, _))) if !bytes.is_empty() => {
                let _ = tokio::fs::create_dir_all(&dir).await;
                let _ = tokio::fs::write(&path, &bytes).await;
            }
            Ok(Err(NexusError::NotFound)) => return Err(ApiError::not_found("no picture")),
            // Keep serving the old one when Nexus doesn't answer
            _ if path.is_file() => {}
            _ => return Err(ApiError::not_found("no picture")),
        }
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("no picture"))?;
    let mime = match bytes.get(..4) {
        Some([0x89, b'P', b'N', b'G']) => "image/png",
        Some([b'R', b'I', b'F', b'F']) => "image/webp",
        Some([b'G', b'I', b'F', _]) => "image/gif",
        Some([b'<', ..]) => "image/svg+xml",
        _ => "image/jpeg",
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Debug, Serialize)]
pub struct Followed {
    pub id: String,
    pub following: bool,
}

/// `PUT` / `DELETE /v1/profiles/<id>/follow`: the standard pubky.app follow on your homeserver.
pub(crate) async fn follow(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<Followed> {
    set_follow(&s, &id, true).await
}

pub(crate) async fn unfollow(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<Followed> {
    set_follow(&s, &id, false).await
}

async fn set_follow(s: &NodeState, id: &str, on: bool) -> ApiResult<Followed> {
    if !valid_id(id) {
        return Err(ApiError::bad_request("that isn't a Pubky key"));
    }
    let sync = s.sync_handle().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Sign in with Pubky first (Settings › Accounts)",
        )
    })?;
    if sync.z32() == id {
        return Err(ApiError::bad_request("that's you"));
    }
    let path = format!("/pub/pubky.app/follows/{id}");
    let r = if on {
        let f = pubky_app_specs::PubkyAppFollow::new();
        sync.homeserver().write_object(sync.session(), &path, &f).await
    } else {
        sync.homeserver().delete(sync.session(), &path).await
    };
    r.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("Your homeserver didn't take it: {e}"),
        )
    })?;
    s.library.lock().expect("library poisoned").set_setting(
        &format!("{FOLLOW_KEY}{id}"),
        &format!("{}:{}", if on { 1 } else { 0 }, pm_library::db::now_unix()),
    )?;
    Ok(Json(Followed {
        id: id.to_string(),
        following: on,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked_and_shortened() {
        let k = "gujx6qd8ksydh1makdphd3bxu351d9b8waqka8hfg6q7hnqkxexo";
        assert!(valid_id(k));
        assert!(!valid_id(
            "pubkygujx6qd8ksydh1makdphd3bxu351d9b8waqka8hfg6q7hnqkxexo"
        ));
        assert!(!valid_id("../etc"));
        assert_eq!(short(k), "gujx…xexo");
    }
}
