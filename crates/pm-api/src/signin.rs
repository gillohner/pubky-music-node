//! One Sign in per source, with the ways side by side (STEWARD-CREDENTIALS-REUSE.md).
//!
//! A source that signs people in describes its ways here: the built-in app, the device-code flow
//! another program uses, a client of your own; later a sign-in imported from another app on this
//! machine and a pasted token. The node remembers which way was picked, says which one is in use,
//! and `/v1/sources` carries one line per source for the Sources overview. Tokens never appear
//! here: a sign-in shows its user and expiry only.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{state::NodeState, ApiError, ApiResult};

/// One way to sign in to a source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Way {
    pub id: String,
    /// Plainly by where it comes from: "Pubky Music's app", "tidal-dl-ng's client", "Your own app".
    pub label: String,
    pub note: String,
    /// `browser` (a sign-in page opens and comes back), `code` (a code to enter on another
    /// device), `setup` (something to enter here first).
    pub action: String,
    #[serde(default)]
    pub recommended: bool,
    pub available: bool,
    /// Why it is not available, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The sign-in a source uses now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InUse {
    pub way: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Unix seconds; the node renews the sign-in itself when the flow allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// What it gives, in plain words ("full tracks", "30-second previews").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gives: Option<String>,
}

/// `GET /v1/sources/{source}/signin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignIn {
    pub source: String,
    /// The recommended way first.
    pub ways: Vec<Way>,
    /// The way picked last, preselected in the chooser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_use: Option<InUse>,
    /// One line for the Sources overview ("tidal-dl-ng's client · full tracks", "Not signed in").
    pub line: String,
    /// Sign-ins found in other apps' files on this machine (`crate::imports`), usable or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<crate::imports::Found>,
    /// Where the node looked ("tidal-dl-ng: ~/.config/tidal_dl_ng/token.json", …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<String>,
    /// The source's own status views, so the tab needs one request.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

pub const NOT_SIGNED_IN: &str = "Not signed in";

/// The setting that remembers the picked way.
pub fn way_key(source: &str) -> String {
    format!("{source}.signin_way")
}

pub(crate) fn chosen(lib: &pm_library::Library, source: &str) -> Option<String> {
    lib.get_setting(&way_key(source))
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
}

/// The overview line for a source that signs people in; `None` for one that does not.
pub(crate) fn line(s: &NodeState, source: &str) -> Option<String> {
    match source {
        crate::tidal::SOURCE_ID => Some(crate::tidal::sign_in(s, None).line),
        _ => None,
    }
}

fn describe(s: &NodeState, source: &str, headers: Option<&HeaderMap>) -> Result<SignIn, ApiError> {
    crate::streaming::find(source)?;
    match source {
        crate::tidal::SOURCE_ID => Ok(crate::tidal::sign_in(s, headers)),
        _ => Err(ApiError::not_found(format!(
            "{source} has no sign-in ways to choose from"
        ))),
    }
}

pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
) -> ApiResult<SignIn> {
    Ok(Json(describe(&s, &source, Some(&headers))?))
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub way: String,
}

/// `PUT /v1/sources/{source}/signin {way}`: remember the way; the source adjusts what it needs
/// (TIDAL: the device way is what "Full tracks" used to switch on).
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
    Json(b): Json<Choice>,
) -> ApiResult<SignIn> {
    let si = describe(&s, &source, Some(&headers))?;
    let Some(w) = si.ways.iter().find(|w| w.id == b.way) else {
        let ids: Vec<&str> = si.ways.iter().map(|w| w.id.as_str()).collect();
        return Err(ApiError::bad_request(format!(
            "no such way: {}; one of {}",
            b.way,
            ids.join(", ")
        )));
    };
    if !w.available {
        return Err(ApiError::bad_request(
            w.reason
                .clone()
                .unwrap_or_else(|| format!("{} is not available on this node", w.label)),
        ));
    }
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(&way_key(&source), &b.way)?;
    if source == crate::tidal::SOURCE_ID {
        crate::tidal::way_chosen(&s, &b.way);
    }
    Ok(Json(describe(&s, &source, Some(&headers))?))
}

#[derive(Debug, Default, Deserialize)]
pub struct ImportBody {
    /// A `Found::id` from the answer's `imports`.
    #[serde(default)]
    pub id: Option<String>,
    /// Or a pasted token file / bare token.
    #[serde(default)]
    pub text: Option<String>,
}

/// `POST /v1/sources/{source}/signin/import {id}` or `{text}`: use a sign-in found in another
/// app's files, or a pasted one. Refused in plain words when it cannot work (expired, not a
/// token file); nothing is changed then.
pub(crate) async fn import(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ImportBody>,
) -> ApiResult<SignIn> {
    crate::streaming::find(&source)?;
    if source != crate::tidal::SOURCE_ID {
        return Err(ApiError::not_found(format!(
            "{source} has no sign-in imports yet"
        )));
    }
    let (imp, way) = match (b.text.as_deref().map(str::trim), b.id.as_deref()) {
        (Some(text), _) if !text.is_empty() => (
            crate::imports::pasted_tidal(text)
                .map_err(|e| ApiError::bad_request(format!("Not usable: {e}")))?,
            crate::tidal::WAY_PASTE,
        ),
        (_, Some(id)) => {
            let imp = crate::imports::scan_tidal()
                .into_iter()
                .find(|i| i.found.id == id)
                .ok_or_else(|| ApiError::not_found(format!("nothing found for {id} on this machine")))?;
            if !imp.found.usable {
                return Err(ApiError::bad_request(format!(
                    "{}'s sign-in is not usable: {}",
                    imp.found.app,
                    imp.found.reason.clone().unwrap_or_default()
                )));
            }
            (imp, crate::tidal::WAY_IMPORT)
        }
        _ => {
            return Err(ApiError::bad_request(
                "Say which sign-in (id) or paste one (text)",
            ))
        }
    };
    crate::tidal::import_sign_in(&s, imp.session);
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(&way_key(&source), way)?;
    Ok(Json(describe(&s, &source, Some(&headers))?))
}

/// `DELETE /v1/sources/{source}/signin`: sign out of the way in use. The picked way stays
/// remembered.
pub(crate) async fn sign_out(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
) -> ApiResult<SignIn> {
    let si = describe(&s, &source, Some(&headers))?;
    if let Some(u) = &si.in_use {
        if source == crate::tidal::SOURCE_ID {
            crate::tidal::sign_out_of(&s, &u.way)?;
        }
    }
    Ok(Json(describe(&s, &source, Some(&headers))?))
}
