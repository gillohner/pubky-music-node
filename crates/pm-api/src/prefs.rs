//! "You" in Settings (STEWARD-SETTINGS-GROUPS.md): a person's own preferences. They override the
//! node's defaults ("This node") for that person, and each answer says which value applies.
//!
//! - The owner's are saved on their homeserver (`/priv/pubky.music/prefs.json`), so they follow
//!   them to every node, with a copy on the node for when the homeserver can't be reached.
//! - A listener's are kept on the node for now (`me.prefs.<key>`): the node has no write session
//!   for their homeserver (ADR 0016 step 5 moves them there).
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

/// The owner's copy on the node.
const OWNER_KEY: &str = "me.prefs";
/// Where a listener's are kept: `me.prefs.<z32>`.
const LISTENER_PREFIX: &str = "me.prefs.";
/// Where the accent colour can come from.
const COLOUR_FROM: &[&str] = &["source", "art"];
/// The node's default for it.
const DEFAULT_COLOUR_FROM: &str = "source";

/// What a person keeps, as stored. Members this build doesn't know are kept on rewrite.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Prefs {
    #[serde(default = "version")]
    pub version: u32,
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
    /// `homeserver` (follows the person to every node) or `node` (only on this node).
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
pub struct PrefsUpdate {
    #[serde(default, deserialize_with = "some_or_null")]
    pub home_layout: Option<Option<HomeLayout>>,
    #[serde(default)]
    pub quality: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    pub appearance: Option<AppearanceUpdate>,
}

#[derive(Debug, Default, Deserialize)]
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
        _ => c
            .key
            .as_ref()
            .map(|k| format!("{LISTENER_PREFIX}{k}"))
            .ok_or_else(|| ApiError::bad_request("preferences need a person: sign in with Pubky Ring")),
    }
}

/// The person's stored preferences (none yet: the defaults).
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

/// The owner's preferences on this node, for the parts of the node that apply them (Home).
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

/// Where the owner's go: the homeserver when the node is signed in with Pubky.
fn saved_on(s: &NodeState, c: &Caller) -> &'static str {
    if c.role == Role::Owner && s.sync_handle().is_some() {
        "homeserver"
    } else {
        "node"
    }
}

/// `GET /v1/me/prefs`: the caller's preferences, each with the value that applies.
pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
) -> ApiResult<PrefsView> {
    storage_key(&c)?;
    let mut p = load(&s, &c);
    // The owner's homeserver copy wins over the node's (set on another node meanwhile)
    if c.role == Role::Owner {
        if let Some(sync) = s.sync_handle() {
            if let Ok(Some(remote)) = sync.read_prefs().await {
                if remote != p {
                    store_local(&s, &c, &remote)?;
                    p = remote;
                }
            }
        }
    }
    Ok(Json(view(&s, &p, saved_on(&s, &c))))
}

fn store_local(s: &NodeState, c: &Caller, p: &Prefs) -> Result<(), ApiError> {
    let key = storage_key(c)?;
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(&key, &serde_json::to_string(p).expect("prefs serialize"))?;
    Ok(())
}

fn valid_id(x: &str) -> bool {
    !x.is_empty()
        && x.len() <= 32
        && x.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `PUT /v1/me/prefs {home_layout?, quality?: {<source>: <id> | null}, appearance?: {colour_from?}}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
    Json(u): Json<PrefsUpdate>,
) -> ApiResult<PrefsView> {
    storage_key(&c)?;
    let mut p = load(&s, &c);
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
                    p.quality.insert(source, id);
                }
                Some(id) => return Err(ApiError::bad_request(format!("{id} is not a quality id"))),
                None => {
                    p.quality.remove(&source);
                }
            }
        }
        if p.quality.len() > 64 {
            return Err(ApiError::bad_request("at most 64 sources"));
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
    store_local(&s, &c, &p)?;
    let mut where_ = "node";
    if c.role == Role::Owner {
        if let Some(sync) = s.sync_handle() {
            match sync.write_prefs(&p).await {
                Ok(()) => where_ = "homeserver",
                // Kept on the node; the next change writes it again
                Err(e) => tracing::warn!("preferences not saved on the homeserver yet: {e}"),
            }
        }
    }
    Ok(Json(view(&s, &p, where_)))
}
