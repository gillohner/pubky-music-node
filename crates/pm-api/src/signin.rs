//! One Sign in per source, with the ways side by side (STEWARD-CREDENTIALS-REUSE.md).
//!
//! A source that signs people in describes its ways here: the built-in app, the device-code flow
//! another program uses, a client of your own, a sign-in imported from another app on this
//! machine, a pasted token. The node remembers which way was picked, says which one is in use,
//! and `/v1/sources` carries one line per source for the Sources overview. Tokens never appear
//! here: a sign-in shows its user and expiry only.
//!
//! Each source implements `SignInSource`; `source` is the list.

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
    /// Plainly by where it comes from: "Pubky Music's app", "Your own device client", "Your own app".
    pub label: String,
    pub note: String,
    /// `browser` (a sign-in page opens and comes back), `code` (a code to enter on another
    /// device), `setup` (something to enter here first), `import` (a found sign-in or a file),
    /// `paste` (a token or a file's contents).
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
    /// One line for the Sources overview ("Your own device client · full tracks", "Not signed in").
    pub line: String,
    /// Sign-ins found in other apps' files on this machine (`crate::imports`), usable or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<crate::imports::Found>,
    /// Where the node looked ("tidal-dl-ng: ~/.config/tidal_dl_ng/token.json", …), or what it
    /// does not look for and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<String>,
    /// The source's own status views, so the tab needs one request.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

pub const NOT_SIGNED_IN: &str = "Not signed in";

/// `POST /v1/sources/{source}/signin/import`: a found sign-in by `id`, or a pasted / picked file.
#[derive(Debug, Default, Deserialize)]
pub struct ImportBody {
    /// A `Found::id` from the answer's `imports`.
    #[serde(default)]
    pub id: Option<String>,
    /// Or a pasted token file / bare token.
    #[serde(default)]
    pub text: Option<String>,
}

/// A source's sign-in ways.
#[async_trait::async_trait]
pub(crate) trait SignInSource: Send + Sync {
    /// With `headers`, the tab's answer (other apps' files are looked at, status views come
    /// along); without, the overview line only.
    fn describe(&self, s: &NodeState, headers: Option<&HeaderMap>) -> SignIn;
    /// A way was picked (not yet signed in with).
    fn way_chosen(&self, _s: &Arc<NodeState>, _way: &str) {}
    /// Sign out of the way in use.
    fn sign_out_of(&self, s: &NodeState, way: &str) -> Result<(), ApiError>;
    /// Use a found sign-in or a pasted / picked file; the way to remember comes back.
    async fn import(&self, s: &Arc<NodeState>, body: ImportBody) -> Result<&'static str, ApiError>;
}

/// The sources that sign people in.
pub(crate) fn source(id: &str) -> Option<Box<dyn SignInSource>> {
    Some(match id {
        crate::tidal::SOURCE_ID => Box::new(crate::tidal::TidalSignIn),
        "qobuz" => Box::new(crate::qobuz::QobuzSignIn),
        "spotify" => Box::new(crate::spotify::SpotifySignIn),
        "youtube" => Box::new(crate::youtube::YouTubeSignIn),
        crate::lastfm::SOURCE_ID => Box::new(crate::lastfm::LastfmSignIn),
        _ => return None,
    })
}

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
    source_of(source).map(|x| x.describe(s, None).line)
}

fn source_of(id: &str) -> Option<Box<dyn SignInSource>> {
    source(id)
}

fn find(id: &str) -> Result<Box<dyn SignInSource>, ApiError> {
    if let Some(x) = source(id) {
        return Ok(x);
    }
    crate::streaming::find(id)?;
    Err(ApiError::not_found(format!(
        "{id} has no sign-in ways to choose from"
    )))
}

fn describe(s: &NodeState, id: &str, headers: Option<&HeaderMap>) -> Result<SignIn, ApiError> {
    Ok(find(id)?.describe(s, headers))
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
    let src = find(&source)?;
    let si = src.describe(&s, Some(&headers));
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
    src.way_chosen(&s, &b.way);
    Ok(Json(src.describe(&s, Some(&headers))))
}

/// `POST /v1/sources/{source}/signin/import {id}` or `{text}`: use a sign-in found in another
/// app's files, or a pasted / picked one. Refused in plain words when it cannot work (expired, not
/// a token file); nothing is changed then.
pub(crate) async fn import(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ImportBody>,
) -> ApiResult<SignIn> {
    let src = find(&source)?;
    if b.id.as_deref().is_none_or(str::is_empty) && b.text.as_deref().is_none_or(|t| t.trim().is_empty()) {
        return Err(ApiError::bad_request(
            "Say which sign-in (id) or paste one (text)",
        ));
    }
    let way = src.import(&s, b).await?;
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(&way_key(&source), way)?;
    Ok(Json(src.describe(&s, Some(&headers))))
}

/// `DELETE /v1/sources/{source}/signin`: sign out of the way in use. The picked way stays
/// remembered.
pub(crate) async fn sign_out(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
) -> ApiResult<SignIn> {
    let src = find(&source)?;
    let si = src.describe(&s, Some(&headers));
    if let Some(u) = &si.in_use {
        src.sign_out_of(&s, &u.way)?;
    }
    Ok(Json(src.describe(&s, Some(&headers))))
}
