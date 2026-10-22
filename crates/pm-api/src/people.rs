//! People on this node and its bundled homeserver (STEWARD-BUNDLED-HOMESERVER.md, step 3): the
//! owner invites someone and picks what they get.
//!
//! - **Homeserver only:** a normal Pubky homeserver account here, with a storage quota.
//! - **Homeserver + music:** the same, and they may also use this node's music service.
//! - **Music only:** someone whose Pubky account is elsewhere, added by their public key.
//!
//! Homeserver invites are signup tokens minted through the admin API, shown as a Pubky Ring link
//! (`pubkyauth://direct_signup?hs=…&st=…`) and QR. A used token names its user (`used_by`), which
//! is how an invite becomes a person. Per person, the owner sets the storage quota, turns the
//! homeserver account on or off (off blocks writes; reads stay) and music access on or off.
//!
//! The node keeps its own list in the library settings (`people`). Who may sign in to the music
//! service is decided by that list; the sign-in itself is the node's Pubky Ring sign-in.

use crate::bundled::{admin, endpoint};
use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

const SETTING: &str = "people";

/// What an invite gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// A Pubky homeserver account here, nothing more.
    Homeserver,
    /// A homeserver account here and this node's music service.
    HomeserverMusic,
    /// The music service only (their account is on another homeserver).
    Music,
}

impl Access {
    pub fn on_homeserver(self) -> bool {
        matches!(self, Self::Homeserver | Self::HomeserverMusic)
    }
    pub fn music(self) -> bool {
        matches!(self, Self::HomeserverMusic | Self::Music)
    }
}

/// An invite the owner made; `key` is filled once someone used it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    pub id: String,
    pub access: Access,
    /// Who it is for, as the owner wrote it.
    #[serde(default)]
    pub name: Option<String>,
    /// The homeserver signup token (homeserver invites).
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub storage_quota_mb: Option<u64>,
    pub created_at: i64,
}

/// Someone with access to this node or its homeserver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    /// Their public key (z32).
    pub key: String,
    pub access: Access,
    #[serde(default)]
    pub name: Option<String>,
    /// Music access on (the owner can take it away without touching their homeserver account).
    pub music_enabled: bool,
    pub added_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Roster {
    #[serde(default)]
    pub people: Vec<Person>,
    #[serde(default)]
    pub invites: Vec<Invite>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn load(s: &NodeState) -> Roster {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

fn store(s: &NodeState, r: &Roster) -> Result<(), ApiError> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(r).expect("serializes"))?;
    Ok(())
}

/// Whether `key` (z32, with or without the `pubky` prefix) may use the music service: the owner
/// always; anyone else while listed with music access on.
pub fn may_use_music(s: &NodeState, key: &str) -> bool {
    let key = key.trim_start_matches("pubky");
    if s.sync_handle()
        .is_some_and(|h| h.z32().trim_start_matches("pubky") == key)
    {
        return true;
    }
    load(s)
        .people
        .iter()
        .any(|p| p.key == key && p.access.music() && p.music_enabled)
}

/// A raw z32 public key (52 characters of the z-base-32 alphabet).
pub(crate) fn valid_key(k: &str) -> Result<String, ApiError> {
    let k = k.trim().trim_start_matches("pubky:").trim_start_matches("pubky");
    if k.len() != 52
        || !k
            .bytes()
            .all(|b| b"ybndrfg8ejkmcpqxot1uwisza345h769".contains(&b))
    {
        return Err(ApiError::bad_request(
            "a Pubky public key is 52 z-base-32 characters",
        ));
    }
    Ok(k.to_string())
}

/// Used homeserver tokens become people: `token → used_by`.
async fn used_tokens(s: &NodeState) -> Result<Vec<(String, String)>, ApiError> {
    let v = admin(s, Method::GET, "/signup_tokens?state=used&limit=1000", None).await?;
    Ok(v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    Some((
                        i["token"].as_str()?.to_string(),
                        i["used_by"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Turns used invites into people (once).
async fn reconcile(s: &NodeState) -> Result<Roster, ApiError> {
    let mut r = load(s);
    if r.invites.iter().all(|i| i.token.is_none()) || s.bundled_homeserver().is_none() {
        return Ok(r);
    }
    let used = used_tokens(s).await?;
    let mut changed = false;
    r.invites.retain(|inv| {
        let Some(t) = &inv.token else { return true };
        let Some((_, key)) = used.iter().find(|(tok, _)| tok == t) else {
            return true;
        };
        if !r.people.iter().any(|p| &p.key == key) {
            r.people.push(Person {
                key: key.clone(),
                access: inv.access,
                name: inv.name.clone(),
                music_enabled: inv.access.music(),
                added_at: now(),
            });
        }
        changed = true;
        false
    });
    if changed {
        store(s, &r)?;
    }
    Ok(r)
}

/// A person as the owner sees them, with their homeserver account's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonView {
    #[serde(flatten)]
    pub person: Person,
    /// Their storage quota on the bundled homeserver (`unlimited`, a number of MB, or the default).
    pub storage_quota_mb: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteView {
    #[serde(flatten)]
    pub invite: Invite,
    /// The Pubky Ring link and its QR code (homeserver invites).
    pub url: Option<String>,
    pub qr_svg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeopleView {
    /// The node's owner (its Pubky identity), when signed in.
    pub owner: Option<String>,
    pub people: Vec<PersonView>,
    pub invites: Vec<InviteView>,
    /// The bundled homeserver as a whole (users, disk used): `GET /v1/homeserver/bundled`.
    pub homeserver: crate::bundled::BundledStatus,
}

async fn homeserver_key(s: &NodeState) -> Result<String, ApiError> {
    let info = admin(s, Method::GET, "/info", None).await?;
    info["public_key"].as_str().map(str::to_string).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "the bundled homeserver has no public key",
        )
    })
}

fn invite_link(hs: &str, token: &str) -> String {
    format!("pubkyauth://direct_signup?hs={hs}&st={token}")
}

fn invite_view(inv: Invite, hs: Option<&str>) -> InviteView {
    let url = match (hs, &inv.token) {
        (Some(hs), Some(t)) => Some(invite_link(hs, t)),
        _ => None,
    };
    InviteView {
        qr_svg: url.as_deref().and_then(crate::qr_svg),
        url,
        invite: inv,
    }
}

/// `GET /v1/people`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<PeopleView> {
    let r = reconcile(&s).await?;
    let homeserver = if s.bundled_homeserver().is_some() {
        match admin(&s, Method::GET, "/info", None).await {
            Ok(v) => crate::bundled::status_from_info(&v),
            Err(e) => crate::bundled::BundledStatus {
                configured: true,
                error: Some(e.message),
                ..Default::default()
            },
        }
    } else {
        Default::default()
    };
    let hs = homeserver.public_key.clone();
    let mut people = Vec::new();
    for p in r.people {
        let quota = if p.access.on_homeserver() && homeserver.reachable {
            admin(&s, Method::GET, &format!("/users/{}/quota", p.key), None)
                .await
                .ok()
                .map(|q| q["effective"]["storage_quota_mb"].clone())
        } else {
            None
        };
        people.push(PersonView {
            person: p,
            storage_quota_mb: quota,
        });
    }
    Ok(Json(PeopleView {
        owner: s.sync_handle().map(|h| h.z32().to_string()),
        people,
        invites: r
            .invites
            .into_iter()
            .map(|i| invite_view(i, hs.as_deref()))
            .collect(),
        homeserver,
    }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewInvite {
    pub access: Access,
    #[serde(default)]
    pub name: Option<String>,
    /// Homeserver invites: their storage in MB; absent = the homeserver's default.
    #[serde(default)]
    pub storage_quota_mb: Option<u64>,
    /// Music-only: their public key.
    #[serde(default)]
    pub key: Option<String>,
}

fn clean_name(n: Option<String>) -> Option<String> {
    n.map(|n| n.trim().chars().take(80).collect::<String>())
        .filter(|n| !n.is_empty())
}

/// `POST /v1/people/invites {access, name?, storage_quota_mb?, key?}`.
/// Homeserver invites answer with the Ring link and QR; a music-only one adds the person by key.
pub(crate) async fn invite(State(s): State<Arc<NodeState>>, Json(b): Json<NewInvite>) -> ApiResult<Value> {
    let name = clean_name(b.name);
    let mut r = load(&s);
    if !b.access.on_homeserver() {
        let key = valid_key(b.key.as_deref().unwrap_or(""))?;
        if let Some(p) = r.people.iter_mut().find(|p| p.key == key) {
            p.music_enabled = true;
            if p.access == Access::Homeserver {
                p.access = Access::HomeserverMusic;
            }
        } else {
            r.people.push(Person {
                key: key.clone(),
                access: Access::Music,
                name,
                music_enabled: true,
                added_at: now(),
            });
        }
        store(&s, &r)?;
        return Ok(Json(serde_json::json!({"person": key})));
    }
    endpoint(&s)?;
    let quota = match b.storage_quota_mb {
        Some(mb) => serde_json::json!({"storage_quota_mb": mb}),
        None => serde_json::json!({}),
    };
    let v = admin(&s, Method::POST, "/generate_signup_token", Some(&quota)).await?;
    let token = v.as_str().unwrap_or_default().to_string();
    if !crate::bundled::is_signup_token(&token) {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "the bundled homeserver did not hand out a signup token",
        ));
    }
    let inv = Invite {
        id: crate::tidal::random_urlsafe(9),
        access: b.access,
        name,
        token: Some(token),
        storage_quota_mb: b.storage_quota_mb,
        created_at: now(),
    };
    r.invites.push(inv.clone());
    store(&s, &r)?;
    let hs = homeserver_key(&s).await?;
    Ok(Json(
        serde_json::to_value(invite_view(inv, Some(&hs))).expect("serializes"),
    ))
}

/// `DELETE /v1/people/invites/{id}`: forgets an unused invite (its token stays valid on the
/// homeserver until used; the admin API cannot revoke one).
pub(crate) async fn forget_invite(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    let mut r = load(&s);
    let before = r.invites.len();
    r.invites.retain(|i| i.id != id);
    if r.invites.len() == before {
        return Err(ApiError::not_found("no such invite"));
    }
    store(&s, &r)?;
    Ok(Json(serde_json::json!({"forgotten": id})))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PersonChange {
    #[serde(default)]
    pub name: Option<String>,
    /// Music access on or off.
    #[serde(default)]
    pub music: Option<bool>,
    /// Their homeserver account on or off (off blocks their writes; reads and deletes stay).
    #[serde(default)]
    pub homeserver_enabled: Option<bool>,
    /// MB, or `"unlimited"`, or `null` for the homeserver's default.
    #[serde(default, deserialize_with = "some_value")]
    pub storage_quota_mb: Option<Value>,
}

/// Keeps an explicit `null` (reset to the default) apart from an absent field.
fn some_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `PATCH /v1/people/{key} {name?, music?, homeserver_enabled?, storage_quota_mb?}`.
pub(crate) async fn change(
    State(s): State<Arc<NodeState>>,
    Path(key): Path<String>,
    Json(b): Json<PersonChange>,
) -> ApiResult<Person> {
    let key = valid_key(&key)?;
    let mut r = reconcile(&s).await?;
    let Some(i) = r.people.iter().position(|p| p.key == key) else {
        return Err(ApiError::not_found("no such person"));
    };
    let on_hs = r.people[i].access.on_homeserver();
    if let Some(q) = &b.storage_quota_mb {
        if !on_hs {
            return Err(ApiError::bad_request(
                "their account is not on this node's homeserver",
            ));
        }
        let ok = q.is_null() || q.as_u64().is_some() || q.as_str() == Some("unlimited");
        if !ok {
            return Err(ApiError::bad_request(
                "storage_quota_mb is a number of MB, \"unlimited\" or null",
            ));
        }
        admin(
            &s,
            Method::PATCH,
            &format!("/users/{key}/quota"),
            Some(&serde_json::json!({"storage_quota_mb": q})),
        )
        .await?;
    }
    if let Some(on) = b.homeserver_enabled {
        if !on_hs {
            return Err(ApiError::bad_request(
                "their account is not on this node's homeserver",
            ));
        }
        let path = format!("/users/{key}/{}", if on { "enable" } else { "disable" });
        admin(&s, Method::POST, &path, None).await?;
    }
    let p = &mut r.people[i];
    if let Some(m) = b.music {
        p.music_enabled = m;
        if m && p.access == Access::Homeserver {
            p.access = Access::HomeserverMusic;
        }
    }
    if b.name.is_some() {
        p.name = clean_name(b.name);
    }
    let out = p.clone();
    store(&s, &r)?;
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_kinds_and_keys() {
        assert!(Access::Homeserver.on_homeserver() && !Access::Homeserver.music());
        assert!(Access::HomeserverMusic.on_homeserver() && Access::HomeserverMusic.music());
        assert!(!Access::Music.on_homeserver() && Access::Music.music());
        let k = "tbepfp8fisyiqpskbmxsmpub47gxb7xdsh5x9ut9qrb97gcq9phy";
        assert_eq!(valid_key(&format!("pubky{k}")).unwrap(), k);
        assert!(valid_key("not-a-key").is_err());
        assert_eq!(
            invite_link("hs1", "AB12-CD34-EF56"),
            "pubkyauth://direct_signup?hs=hs1&st=AB12-CD34-EF56"
        );
        let c: PersonChange = serde_json::from_str(r#"{"storage_quota_mb": null}"#).unwrap();
        assert_eq!(
            c.storage_quota_mb,
            Some(Value::Null),
            "null resets to the default"
        );
        let c: PersonChange = serde_json::from_str(r#"{"music": false}"#).unwrap();
        assert!(c.storage_quota_mb.is_none());
    }
}
