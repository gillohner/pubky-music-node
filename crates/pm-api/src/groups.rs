//! Settings in four groups (STEWARD-SETTINGS-GROUPS.md), the same in the UI, the API and
//! docs/APP-DATA.md. Each setting lives in exactly one group:
//!
//! - **You** (`/v1/me/prefs`, `/v1/vault`): saved on the person's homeserver, follows them.
//! - **This device**: kept by the app on the device, never on the node (no API).
//! - **This node** (admin): service apps, people, library folders, zones and outputs, remote
//!   access, updates.
//! - **This homeserver** (admin, only with a bundled one): `/v1/homeserver/bundled/*`.
//!
//! `GET /v1/node` lists them with the routes behind each section, so the UI and the docs don't
//! each keep their own map.

use crate::state::NodeState;
use crate::ApiResult;
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

/// `GET /v1/node` (owner): the settings groups, where each is saved, and the routes behind each
/// section. "This homeserver" is listed only when the node bundles one.
pub(crate) async fn index(State(s): State<Arc<NodeState>>) -> ApiResult<Value> {
    let mut groups = vec![
        json!({
            "id": "you", "title": "You",
            "saved": "Saved on your homeserver: follows you to every node and device",
            "sections": [
                {"id": "accounts", "title": "My accounts", "routes": ["/v1/vault"]},
                {"id": "home", "title": "Home and layout", "routes": ["/v1/me/prefs"]},
                {"id": "quality", "title": "Playback quality", "routes": ["/v1/me/prefs"]},
                {"id": "appearance", "title": "Appearance", "routes": ["/v1/me/prefs"]},
            ],
        }),
        json!({
            "id": "device", "title": "This device",
            "saved": "Only on this device",
            "sections": [
                {"id": "dac", "title": "USB DAC and bit-perfect", "routes": []},
                {"id": "downloads", "title": "Downloads and offline", "routes": ["/v1/devices"]},
                {"id": "cache", "title": "Cache", "routes": []},
            ],
        }),
        json!({
            "id": "node", "title": "This node",
            "saved": "For everyone on this node",
            "sections": [
                {"id": "apps", "title": "Service apps", "routes": ["/v1/sources"]},
                {"id": "people", "title": "People", "routes": ["/v1/people", "/v1/people/invites", "/v1/people/zones"]},
                {"id": "folders", "title": "Library folders", "routes": ["/v1/library/folders"]},
                {"id": "zones", "title": "Zones and outputs", "routes": ["/v1/zones", "/v1/outputs"]},
                // The iroh link is set in node.toml (`[remote]`); no route yet
                {"id": "remote", "title": "Remote access", "routes": []},
                {"id": "home", "title": "Home for everyone (the default)", "routes": ["/v1/home/layout"]},
                {"id": "backup", "title": "Backup", "routes": ["/v1/backup", "/v1/node/settings/backup"]},
            ],
        }),
    ];
    if s.bundled_homeserver().is_some() {
        groups.push(json!({
            "id": "homeserver", "title": "This homeserver",
            "saved": "The homeserver this node runs",
            "sections": [
                {"id": "status", "title": "Users and storage", "routes": ["/v1/homeserver/bundled"]},
                {"id": "invites", "title": "Sign-ups and invites", "routes": ["/v1/people/invites"]},
                {"id": "tunnel", "title": "Domain and tunnel", "routes": ["/v1/homeserver/bundled/tunnel"]},
            ],
        }));
    }
    Ok(Json(json!({ "groups": groups })))
}
