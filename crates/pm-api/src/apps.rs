//! "Service apps" in Settings › This node (ADR 0016 §4): every app the admin sets once for the
//! whole node, on one screen. A person's own sign-ins are elsewhere ("You", the vault).
//!
//! `GET /v1/node/apps` lists each one: whether it is set, where the value comes from, where to get
//! one, and the route that sets it. Never a secret, nor the ids that are kept private.

use crate::state::NodeState;
use crate::ApiResult;
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

/// One node app.
#[derive(Debug, Serialize, PartialEq)]
pub struct NodeApp {
    pub id: &'static str,
    pub name: &'static str,
    pub configured: bool,
    /// `settings` (entered here), `config` (node.toml or the environment), `builtin` (this build's
    /// own), or none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The route that sets or clears it, or none when it is set in node.toml only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set: Option<&'static str>,
    /// The route that says whether it works, where there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    /// Where to get one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get_one: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

fn app(id: &'static str, name: &'static str, configured: bool, from: Option<String>) -> NodeApp {
    NodeApp {
        id,
        name,
        configured,
        from: if configured { from } else { None },
        set: None,
        status: None,
        get_one: None,
        note: None,
    }
}

/// Every node app, as `GET /v1/node/apps` lists it.
pub fn list(s: &NodeState) -> Vec<NodeApp> {
    // A value stored before origins were recorded has none: it was entered in Settings
    let origin = |f: String| if f.is_empty() { "settings".to_string() } else { f };
    let (qobuz, soundcloud, lastfm, tidal_full, acoustid_own) = {
        let lib = s.library.lock().expect("library poisoned");
        (
            crate::qobuz::app_of(&lib).map(|a| origin(a.from)),
            crate::soundcloud::client_of(&lib).map(|c| origin(c.from)),
            crate::lastfm::app_of(&lib).map(|a| origin(a.from)),
            crate::tidal_unofficial::client_of(&lib).map(|c| origin(c.from)),
            lib.get_setting(crate::identify::KEY_SETTING)
                .ok()
                .flatten()
                .is_some_and(|k| !k.trim().is_empty()),
        )
    };
    let tidal = crate::tidal::app_id(s);
    let tidal_from = tidal.as_deref().map(|id| {
        if crate::project_apps::tidal_client_id() == Some(id) {
            "builtin"
        } else {
            "config"
        }
    });
    // The build's own app where the value is the project's, else the node's config
    let built_or_config = |id: &str, built: Option<&str>| {
        if built == Some(id) {
            "builtin".to_string()
        } else {
            "config".to_string()
        }
    };
    let (google, onedrive) = {
        let d = s.drives.read().expect("drives");
        let from = |p: crate::drives::Provider, built: Option<&'static str>| {
            d.apps.get(&p).map(|a| built_or_config(&a.client_id, built))
        };
        (
            from(
                crate::drives::Provider::Google,
                option_env!("PM_PROJECT_GOOGLE_CLIENT_ID"),
            ),
            from(
                crate::drives::Provider::Onedrive,
                option_env!("PM_PROJECT_ONEDRIVE_CLIENT_ID"),
            ),
        )
    };
    let acoustid_from = {
        let a = s.acoustid.read().expect("acoustid");
        if a.api_key.trim().is_empty() {
            None
        } else if acoustid_own {
            Some("settings".to_string())
        } else {
            Some(built_or_config(&a.api_key, crate::project_apps::acoustid_key()))
        }
    };
    let jamendo = {
        let j = s.jamendo.read().expect("jamendo");
        (!j.creds.client_id.trim().is_empty())
            .then(|| built_or_config(&j.creds.client_id, crate::project_apps::jamendo_client_id()))
    };
    let spotify = crate::spotify::app_origin(s);
    let mut out = vec![
        NodeApp {
            // node.toml wins over Settings: the route refuses then
            set: (spotify != Some("config")).then_some("PUT /v1/sources/spotify/app"),
            status: Some("GET /v1/sources/spotify/status"),
            get_one: Some("https://developer.spotify.com/dashboard"),
            note: Some("In development mode Spotify lets 25 people in: add each one under User Management"),
            ..app("spotify", "Spotify", spotify.is_some(), spotify.map(str::to_string))
        },
        NodeApp {
            status: Some("GET /v1/sources/tidal/status"),
            get_one: Some("https://developer.tidal.com"),
            note: Some("Set in node.toml ([tidal] client_id)"),
            ..app("tidal", "TIDAL", tidal.is_some(), tidal_from.map(str::to_string))
        },
        NodeApp {
            set: Some("PUT /v1/sources/tidal/unofficial"),
            status: Some("GET /v1/sources/tidal/unofficial"),
            note: Some(
                "A TIDAL device client (full tracks); an app from TIDAL's developer portal doesn't work here",
            ),
            ..app(
                "tidal-full",
                "TIDAL full tracks",
                tidal_full.is_some(),
                tidal_full,
            )
        },
        NodeApp {
            set: Some("PUT /v1/sources/qobuz/unofficial"),
            status: Some("GET /v1/sources/qobuz/unofficial"),
            ..app("qobuz", "Qobuz", qobuz.is_some(), qobuz)
        },
        NodeApp {
            set: Some("PUT /v1/sources/soundcloud/unofficial"),
            status: Some("GET /v1/sources/soundcloud/unofficial"),
            ..app("soundcloud", "SoundCloud", soundcloud.is_some(), soundcloud)
        },
        NodeApp {
            set: Some("PUT /v1/sources/youtube/key"),
            status: Some("GET /v1/sources/youtube/key"),
            get_one: Some("https://console.cloud.google.com/apis/library/youtube.googleapis.com"),
            note: Some("Checked with YouTube when it is saved"),
            ..app(
                "youtube",
                "YouTube Data",
                crate::youtube::has_key(s),
                Some("settings".into()),
            )
        },
        NodeApp {
            set: Some("PUT /v1/lastfm"),
            status: Some("GET /v1/lastfm"),
            get_one: Some("https://www.last.fm/api/account/create"),
            ..app("lastfm", "Last.fm", lastfm.is_some(), lastfm)
        },
        NodeApp {
            status: Some("GET /v1/sources/jamendo/status"),
            get_one: Some("https://devportal.jamendo.com"),
            note: Some("Set in node.toml ([jamendo] client_id) or PM_JAMENDO_CLIENT_ID"),
            ..app("jamendo", "Jamendo", jamendo.is_some(), jamendo)
        },
        NodeApp {
            set: Some("PUT /v1/library/identify/settings"),
            status: Some("GET /v1/library/identify"),
            get_one: Some("https://acoustid.org/new-application"),
            ..app(
                "acoustid",
                "AcoustID",
                acoustid_from.is_some(),
                acoustid_from,
            )
        },
        NodeApp {
            status: Some("GET /v1/library/drives"),
            note: Some("Set in node.toml ([drives] google_client_id, google_client_secret) or PM_GOOGLE_DRIVE_CLIENT_ID / _SECRET"),
            ..app("google-drive", "Google Drive", google.is_some(), google)
        },
        NodeApp {
            status: Some("GET /v1/library/drives"),
            note: Some("Set in node.toml ([drives] onedrive_client_id) or PM_ONEDRIVE_CLIENT_ID"),
            ..app("onedrive", "OneDrive", onedrive.is_some(), onedrive)
        },
        NodeApp {
            note: Some("Artist images; this build's key"),
            ..app(
                "fanart",
                "fanart.tv",
                crate::project_apps::fanart_key().is_some(),
                Some("builtin".into()),
            )
        },
    ];
    // TheAudioDB always answers (its public test key without a project key)
    out.push(NodeApp {
        note: Some("Artist biographies; this build's key, or TheAudioDB's public test key"),
        ..app("audiodb", "TheAudioDB", true, Some("builtin".into()))
    });
    out
}

/// `GET /v1/node/apps` (owner): every app the node uses, set once for everyone.
pub(crate) async fn get(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<NodeApp>> {
    Ok(Json(list(&s)))
}
