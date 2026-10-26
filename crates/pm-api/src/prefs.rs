//! "You" in Settings (the owner's settings-groups request): a person's own preferences. They
//! override the node's defaults ("This node") for that person, and each answer says which value
//! applies.
//!
//! - The owner's are saved on their homeserver (`/priv/pubky.music/prefs.json`), so they follow
//!   them to every node, with a copy on the node for when the homeserver can't be reached.
//! - A listener's are kept on the node for now (`me.prefs.<key>`): the node has no write session
//!   for their homeserver (ADR 0016 step 5 moves them there).
//!
//! The newer copy wins, by `updated_at`: a change made while the homeserver was unreachable is
//! kept on the node, marked as not saved there yet, and written at the next GET or PUT; a copy on
//! the homeserver that another node changed later replaces the node's. Every call to the
//! homeserver is time-limited, so Settings answers from the node's copy when it is slow.
//!
//! What is in it: the Home layout (node default: `home.layout`), the streaming quality per
//! source (node default: the source's own setting, `GET /v1/sources/<id>/quality` once it lands),
//! and appearance (where the accent colour comes from: the source or the album art).

use crate::accounts::{Caller, Role};
use crate::home::HomeLayout;
use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The owner's copy on the node.
const OWNER_KEY: &str = "me.prefs";
/// Set while the owner's copy on the node isn't on their homeserver yet.
const OWNER_PENDING_KEY: &str = "me.prefs_pending";
/// Where a listener's are kept: `me.prefs.<z32>`.
const LISTENER_PREFIX: &str = "me.prefs.";
/// Where the accent colour can come from.
const COLOUR_FROM: &[&str] = &["source", "art"];
/// The node's default for it.
const DEFAULT_COLOUR_FROM: &str = "source";
/// The most sources a person sets a quality for.
const MAX_QUALITY: usize = 64;
/// The longest a call to the homeserver may take before Settings answers without it.
const HOMESERVER_FOR: Duration = Duration::from_secs(3);

/// What a person keeps, as stored. Members this build doesn't know are kept on rewrite.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Prefs {
    #[serde(default = "version")]
    pub version: u32,
    /// When it was last changed (Unix milliseconds, the changing node's clock): the newer copy
    /// wins between the node and the homeserver.
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_layout: Option<HomeLayout>,
    /// Source id → quality id (the ids of that source's quality choice).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quality: BTreeMap<String, String>,
    #[serde(default)]
    pub appearance: Appearance,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            version: version(),
            updated_at: 0,
            home_layout: None,
            quality: BTreeMap::new(),
            appearance: Appearance::default(),
            extra: Map::new(),
        }
    }
}

fn version() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Appearance {
    /// `source` or `art`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour_from: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One value as Settings shows it: what applies, and whose it is.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Applied<T> {
    pub value: T,
    /// `yours` (set in "You") or `node default` (from "This node").
    pub applies: &'static str,
}

fn yours<T>(value: T) -> Applied<T> {
    Applied {
        value,
        applies: "yours",
    }
}

fn node_default<T>(value: T) -> Applied<T> {
    Applied {
        value,
        applies: "node default",
    }
}

/// `GET /v1/me/prefs`.
#[derive(Debug, Serialize)]
pub struct PrefsView {
    /// `homeserver` (follows the person to every node) or `node` (only on this node, or not saved
    /// on the homeserver yet).
    pub saved_on: &'static str,
    pub home_layout: Applied<HomeLayout>,
    /// Only the sources the person chose a quality for; the others follow the node's setting.
    pub quality: BTreeMap<String, Applied<String>>,
    pub appearance: AppearanceView,
}

#[derive(Debug, Serialize)]
pub struct AppearanceView {
    pub colour_from: Applied<String>,
}

/// `PUT /v1/me/prefs`: only the members given change; `null` drops an override (the node's
/// default applies again).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefsUpdate {
    #[serde(default, deserialize_with = "some_or_null")]
    pub home_layout: Option<Option<HomeLayout>>,
    #[serde(default)]
    pub quality: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    pub appearance: Option<AppearanceUpdate>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceUpdate {
    #[serde(default, deserialize_with = "some_or_null")]
    pub colour_from: Option<Option<String>>,
}

/// A member that is absent stays `None`; one that is `null` becomes `Some(None)`.
fn some_or_null<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

fn storage_key(c: &Caller) -> Result<String, ApiError> {
    match c.role {
        Role::Owner => Ok(OWNER_KEY.to_string()),
        Role::Listener => c
            .key
            .as_ref()
            .map(|k| format!("{LISTENER_PREFIX}{k}"))
            .ok_or_else(|| ApiError::bad_request("preferences need a person: sign in with Pubky Ring")),
        Role::Guest => Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "a party guest has no preferences",
        )),
    }
}

/// The person's stored preferences on the node (none yet: the defaults).
pub fn load(s: &NodeState, c: &Caller) -> Prefs {
    let Ok(key) = storage_key(c) else {
        return Prefs::default();
    };
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(&key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// The owner's preferences on this node, for the parts of the node that apply them.
pub fn owner(s: &NodeState) -> Prefs {
    load(
        s,
        &Caller {
            role: Role::Owner,
            key: None,
        },
    )
}

fn node_layout(s: &NodeState) -> HomeLayout {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(crate::home::LAYOUT_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn view(s: &NodeState, p: &Prefs, saved_on: &'static str) -> PrefsView {
    PrefsView {
        saved_on,
        home_layout: match &p.home_layout {
            Some(l) => yours(l.clone()),
            None => node_default(node_layout(s)),
        },
        quality: p
            .quality
            .iter()
            .map(|(k, v)| (k.clone(), yours(v.clone())))
            .collect(),
        appearance: AppearanceView {
            colour_from: match &p.appearance.colour_from {
                Some(c) => yours(c.clone()),
                None => node_default(DEFAULT_COLOUR_FROM.to_string()),
            },
        },
    }
}

fn store_local(s: &NodeState, c: &Caller, p: &Prefs) -> Result<(), ApiError> {
    let key = storage_key(c)?;
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(&key, &serde_json::to_string(p).expect("prefs serialize"))?;
    Ok(())
}

fn pending(s: &NodeState) -> bool {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(OWNER_PENDING_KEY)
        .ok()
        .flatten()
        .is_some()
}

fn set_pending(s: &NodeState, on: bool) {
    let lib = s.library.lock().expect("library poisoned");
    let _ = if on {
        lib.set_setting(OWNER_PENDING_KEY, "1")
    } else {
        lib.delete_setting(OWNER_PENDING_KEY).map(|_| ())
    };
}

/// The owner's copy on their homeserver: `Ok(None)` when there is none, `Err` when it couldn't be
/// read in time.
async fn remote(sync: &crate::sync::PubkySync) -> Result<Option<Prefs>, String> {
    match tokio::time::timeout(HOMESERVER_FOR, sync.read_prefs()).await {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the homeserver didn't answer in time".into()),
    }
}

/// Write the owner's copy to their homeserver; whether it got there.
async fn push(sync: &crate::sync::PubkySync, p: &Prefs) -> bool {
    match tokio::time::timeout(HOMESERVER_FOR, sync.write_prefs(p)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!("preferences not saved on the homeserver yet: {e}");
            false
        }
        Err(_) => {
            tracing::warn!("preferences not saved on the homeserver yet: it didn't answer in time");
            false
        }
    }
}

/// One change of the owner's preferences at a time, so two writes can't land out of order.
fn owner_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The owner's preferences, settled between the node and the homeserver: the newer copy wins,
/// and one not saved there yet goes there now. Returns them and where they are saved.
async fn settle_owner(s: &NodeState, c: &Caller) -> Result<(Prefs, &'static str), ApiError> {
    let local = load(s, c);
    let Some(sync) = s.sync_handle() else {
        return Ok((local, "node"));
    };
    let unsaved = pending(s);
    match remote(&sync).await {
        Ok(Some(r)) if r.updated_at > local.updated_at && !unsaved => {
            store_local(s, c, &r)?;
            Ok((r, "homeserver"))
        }
        Ok(r) => {
            // The node's is newer, or the homeserver has none, or the node's wasn't saved there
            let behind = unsaved || r.as_ref() != Some(&local);
            if behind && local.updated_at > 0 {
                if push(&sync, &local).await {
                    set_pending(s, false);
                    return Ok((local, "homeserver"));
                }
                return Ok((local, "node"));
            }
            Ok((local, "homeserver"))
        }
        Err(e) => {
            tracing::warn!("preferences: answering from the node's copy: {e}");
            Ok((local, "node"))
        }
    }
}

/// `GET /v1/me/prefs`: the caller's preferences, each with the value that applies.
pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
) -> ApiResult<PrefsView> {
    storage_key(&c)?;
    if c.role == Role::Owner {
        let _one = owner_lock().lock().await;
        let (p, saved_on) = settle_owner(&s, &c).await?;
        return Ok(Json(view(&s, &p, saved_on)));
    }
    Ok(Json(view(&s, &load(&s, &c), "node")))
}

fn valid_id(x: &str) -> bool {
    !x.is_empty()
        && x.len() <= 32
        && x.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Apply a change to `p` (refused as a whole when any part is invalid).
fn apply(p: &mut Prefs, u: PrefsUpdate) -> Result<(), ApiError> {
    if let Some(l) = u.home_layout {
        if let Some(l) = &l {
            crate::home::check_layout(l)?;
        }
        p.home_layout = l;
    }
    if let Some(q) = u.quality {
        for (source, id) in q {
            if !valid_id(&source) {
                return Err(ApiError::bad_request(format!("{source} is not a source id")));
            }
            match id {
                Some(id) if valid_id(&id) => {
                    if !p.quality.contains_key(&source) && p.quality.len() >= MAX_QUALITY {
                        return Err(ApiError::bad_request(format!("at most {MAX_QUALITY} sources")));
                    }
                    p.quality.insert(source, id);
                }
                Some(id) => return Err(ApiError::bad_request(format!("{id} is not a quality id"))),
                None => {
                    p.quality.remove(&source);
                }
            }
        }
    }
    if let Some(a) = u.appearance {
        if let Some(c) = a.colour_from {
            if let Some(c) = &c {
                if !COLOUR_FROM.contains(&c.as_str()) {
                    return Err(ApiError::bad_request("colour_from is source or art"));
                }
            }
            p.appearance.colour_from = c;
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `PUT /v1/me/prefs {home_layout?, quality?: {<source>: <id> | null}, appearance?: {colour_from?}}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
    Json(u): Json<PrefsUpdate>,
) -> ApiResult<PrefsView> {
    storage_key(&c)?;
    if c.role != Role::Owner {
        let mut p = load(&s, &c);
        apply(&mut p, u)?;
        p.updated_at = now_ms();
        store_local(&s, &c, &p)?;
        return Ok(Json(view(&s, &p, "node")));
    }
    let _one = owner_lock().lock().await;
    // The change goes on top of the newer of the node's copy and the homeserver's, so a change
    // made on another node meanwhile isn't written over
    let (mut p, _) = settle_owner(&s, &c).await?;
    apply(&mut p, u)?;
    p.updated_at = now_ms().max(p.updated_at + 1);
    store_local(&s, &c, &p)?;
    let Some(sync) = s.sync_handle() else {
        return Ok(Json(view(&s, &p, "node")));
    };
    set_pending(&s, true);
    let saved_on = if push(&sync, &p).await {
        set_pending(&s, false);
        "homeserver"
    } else {
        "node"
    };
    Ok(Json(view(&s, &p, saved_on)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_quality_list_can_still_be_trimmed_and_changed() {
        let mut p = Prefs::default();
        assert_eq!(p.version, 1);
        for i in 0..MAX_QUALITY {
            p.quality.insert(format!("s{i}"), "max".into());
        }
        let more: PrefsUpdate =
            serde_json::from_value(serde_json::json!({"quality": {"new": "max"}})).unwrap();
        assert!(apply(&mut p.clone(), more).is_err(), "a 65th source");
        let change: PrefsUpdate =
            serde_json::from_value(serde_json::json!({"quality": {"s1": "cd", "s2": null}})).unwrap();
        apply(&mut p, change).unwrap();
        assert_eq!(p.quality["s1"], "cd");
        assert_eq!(p.quality.len(), MAX_QUALITY - 1);
        // Nothing is changed when one part is refused
        let mixed: PrefsUpdate = serde_json::from_value(
            serde_json::json!({"quality": {"s3": "cd"}, "appearance": {"colour_from": "x"}}),
        )
        .unwrap();
        let mut q = p.clone();
        assert!(apply(&mut q, mixed).is_err());
    }
}
