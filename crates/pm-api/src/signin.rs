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

use crate::{accounts::Who, state::NodeState, ApiError, ApiResult};

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

/// A source's sign-in ways, for one person (each signs in to their own account).
#[async_trait::async_trait]
pub(crate) trait SignInSource: Send + Sync {
    /// With `headers`, the tab's answer (other apps' files are looked at, status views come
    /// along); without, the overview line only.
    fn describe(&self, s: &NodeState, who: &Who, headers: Option<&HeaderMap>) -> SignIn;
    /// A way was picked (not yet signed in with).
    fn way_chosen(&self, _s: &Arc<NodeState>, _who: &Who, _way: &str) {}
    /// Sign out of the way in use.
    fn sign_out_of(&self, s: &NodeState, who: &Who, way: &str) -> Result<(), ApiError>;
    /// Use a found sign-in or a pasted / picked file; the way to remember comes back.
    async fn import(&self, s: &Arc<NodeState>, who: &Who, body: ImportBody)
        -> Result<&'static str, ApiError>;
    /// Each person signs in on their own (false: the node's one sign-in, the owner's).
    fn per_person(&self) -> bool {
        true
    }
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

/// The way this person picked.
pub(crate) fn chosen(lib: &pm_library::Library, who: &Who, source: &str) -> Option<String> {
    who.get(lib, &way_key(source)).filter(|v| !v.is_empty())
}

/// The overview line for a source that signs people in, for whoever is asking; `None` for one
/// that does not.
pub(crate) fn line(s: &NodeState, source: &str) -> Option<String> {
    let who = crate::accounts::asking();
    source_of(source)
        .filter(|x| x.per_person() || who.is_owner())
        .map(|x| x.describe(s, &who, None).line)
}

fn source_of(id: &str) -> Option<Box<dyn SignInSource>> {
    source(id)
}

/// The source's sign-in for this person: a source with one node-wide sign-in is the owner's.
fn find_for(id: &str, who: &Who) -> Result<Box<dyn SignInSource>, ApiError> {
    let x = find(id)?;
    if !x.per_person() && !who.is_owner() {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "that sign-in is the node owner's",
        ));
    }
    if *who == Who::Nobody {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "sign in to the node first: sign-ins are kept per person",
        ));
    }
    Ok(x)
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

pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    Path(source): Path<String>,
    headers: HeaderMap,
) -> ApiResult<SignIn> {
    let who = Who::of(&caller);
    Ok(Json(find_for(&source, &who)?.describe(&s, &who, Some(&headers))))
}

/// One service in Settings › You › Services: the caller's own sign-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyService {
    pub source: String,
    pub name: String,
    /// The caller is signed in to their own account.
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The way in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub way: Option<String>,
    /// Some way to sign in is open to the caller (false: the owner hasn't set up what it needs).
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Where the sign-in ways are: `GET/PUT/DELETE` it.
    pub setup: String,
}

/// The services each person signs in to on their own.
pub(crate) const PER_PERSON: &[(&str, &str)] = &[
    (crate::tidal::SOURCE_ID, "TIDAL"),
    ("qobuz", "Qobuz"),
    ("spotify", "Spotify"),
];

/// `GET /v1/me/services`: the caller's own streaming sign-ins (owner or listener), for
/// Settings › You › Services. A source the caller isn't connected to plays nothing for them.
pub(crate) async fn my_services(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
) -> ApiResult<Vec<MyService>> {
    let who = Who::of(&caller);
    if who == Who::Nobody {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "sign in to the node first: sign-ins are kept per person",
        ));
    }
    let mut out = Vec::new();
    for (id, name) in PER_PERSON {
        let Some(src) = source(id) else { continue };
        let si = src.describe(&s, &who, None);
        let available = si.ways.iter().any(|w| w.available);
        out.push(MyService {
            source: (*id).into(),
            name: (*name).into(),
            connected: si.in_use.is_some(),
            user: si.in_use.as_ref().and_then(|u| u.user.clone()),
            way: si.in_use.as_ref().map(|u| u.way.clone()),
            available,
            reason: (!available)
                .then(|| si.ways.iter().find_map(|w| w.reason.clone()))
                .flatten(),
            setup: format!("/v1/sources/{id}/signin"),
        });
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub way: String,
}

/// `PUT /v1/sources/{source}/signin {way}`: remember the way; the source adjusts what it needs
/// (TIDAL: the device way is what "Full tracks" used to switch on).
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    Path(source): Path<String>,
    headers: HeaderMap,
    Json(b): Json<Choice>,
) -> ApiResult<SignIn> {
    let who = Who::of(&caller);
    let src = find_for(&source, &who)?;
    let si = src.describe(&s, &who, Some(&headers));
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
    who.put(
        &s.library.lock().expect("library poisoned"),
        &way_key(&source),
        Some(&b.way),
    )?;
    src.way_chosen(&s, &who, &b.way);
    Ok(Json(src.describe(&s, &who, Some(&headers))))
}

/// `POST /v1/sources/{source}/signin/import {id}` or `{text}`: use a sign-in found in another
/// app's files, or a pasted / picked one. Refused in plain words when it cannot work (expired, not
/// a token file); nothing is changed then.
pub(crate) async fn import(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    Path(source): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ImportBody>,
) -> ApiResult<SignIn> {
    let who = Who::of(&caller);
    let src = find_for(&source, &who)?;
    // Other apps' files on the node's machine are the owner's: a listener pastes their own
    if b.id.is_some() && !who.is_owner() {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "the sign-ins found on this machine are the owner's; paste your own instead",
        ));
    }
    if b.id.as_deref().is_none_or(str::is_empty) && b.text.as_deref().is_none_or(|t| t.trim().is_empty()) {
        return Err(ApiError::bad_request(
            "Say which sign-in (id) or paste one (text)",
        ));
    }
    let way = src.import(&s, &who, b).await?;
    who.put(
        &s.library.lock().expect("library poisoned"),
        &way_key(&source),
        Some(way),
    )?;
    Ok(Json(src.describe(&s, &who, Some(&headers))))
}

/// `DELETE /v1/sources/{source}/signin`: sign out of the way in use. The picked way stays
/// remembered.
pub(crate) async fn sign_out(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    Path(source): Path<String>,
    headers: HeaderMap,
) -> ApiResult<SignIn> {
    let who = Who::of(&caller);
    let src = find_for(&source, &who)?;
    let si = src.describe(&s, &who, Some(&headers));
    if let Some(u) = &si.in_use {
        src.sign_out_of(&s, &who, &u.way)?;
    }
    Ok(Json(src.describe(&s, &who, Some(&headers))))
}
