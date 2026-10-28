//! `GET /v1/me/first-steps`: what Home's "Get started" card shows the caller. Only the steps that
//! matter on this node and to this caller, and whether the card shows at all: not when the owner
//! hid it (`PUT /v1/node/settings {first_steps_hidden}`, kept on the node), and not once every step
//! that isn't optional is done.
//!
//! - `music` (the owner): add a music folder. Not shown when the node already has a folder (from
//!   the config, added, a mounted `/music`), a WebDAV share or cloud drive, or plays from a
//!   service someone signed in to or a server of their own (streaming only). Done: tracks in the
//!   library.
//! - `play` (everyone): play something. Done: anything played on the node.
//! - `contact` (the owner): the contact Discover needs. Done: one is set.
//! - `speakers` (the owner, optional): only while the node has no output but the browser.
//! - `pubky` (the owner, optional): sign the node in with Pubky Ring. Done: signed in.

use crate::accounts::{Caller, Role};
use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::{Extension, Json};
use std::sync::Arc;

/// Library setting: the owner hid the card ("1").
pub(crate) const HIDDEN_KEY: &str = "ui.first_steps_hidden";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Step {
    /// `music`, `play`, `contact`, `speakers` or `pubky`.
    pub id: &'static str,
    pub done: bool,
    /// The card goes away without it.
    pub optional: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FirstSteps {
    /// Whether Home shows the card.
    pub show: bool,
    /// The owner hid it (Settings › Home for everyone shows it again).
    pub hidden: bool,
    /// The steps that matter here, in order (done ones too, ticked off).
    pub steps: Vec<Step>,
}

/// Sources that play without anyone signing in or setting anything up: having them says nothing
/// about where this node's music comes from.
const NO_ACCOUNT: &[&str] = &["archive", "audius", "jamendo", "podcasts", "radio"];

/// The node plays from a service someone signed in to, or from a server of their own.
fn streams_from_an_account(s: &NodeState) -> bool {
    crate::streaming::services().iter().any(|svc| {
        let id = svc.id();
        if NO_ACCOUNT.contains(&id) || !s.source_enabled(id) {
            return false;
        }
        let status = svc.info(s).status;
        // Deezer's previews and YouTube in the browser need no account either
        crate::streaming::healthy_status(&status)
            && !status.starts_with("previews")
            && !status.starts_with("browser")
    })
}

pub(crate) fn first_steps(s: &NodeState, role: Role) -> Result<FirstSteps, ApiError> {
    let (tracks, played, hidden) = {
        let lib = s.library.lock().expect("library poisoned");
        (
            lib.stats()?.tracks,
            !lib.history(1)?.is_empty(),
            lib.get_setting(HIDDEN_KEY)?.is_some_and(|v| v == "1"),
        )
    };
    let step = |id, done, optional| Step { id, done, optional };
    let mut steps = Vec::new();
    let owner = role == Role::Owner;
    if owner {
        let has_place = !s.all_folders().is_empty()
            || !s.webdav.read().expect("webdav").is_empty()
            || !s.drives.read().expect("drives").roots.is_empty();
        if !has_place && !streams_from_an_account(s) {
            steps.push(step("music", tracks > 0, false));
        }
    }
    steps.push(step("play", played, false));
    if owner {
        steps.push(step("contact", !s.contact().trim().is_empty(), false));
        let speakers = s.outputs.list().iter().any(|o| !o.id.0.starts_with("null"));
        if !speakers {
            steps.push(step("speakers", false, true));
        }
        steps.push(step("pubky", s.sync_handle().is_some(), true));
    }
    let show = !hidden && steps.iter().any(|x| !x.done && !x.optional);
    Ok(FirstSteps { show, hidden, steps })
}

/// `GET /v1/me/first-steps`.
pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
) -> ApiResult<FirstSteps> {
    let st = s.clone();
    let out = tokio::task::spawn_blocking(move || first_steps(&st, c.role))
        .await
        .map_err(|e| ApiError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(out))
}
