//! "Service apps" in Settings › This node (ADR 0016 §4): every app the admin sets once for the
//! whole node, on one screen. A person's own sign-ins are elsewhere ("You", the vault).
//!
//! `GET /v1/node/apps` lists each one: whether it is set, where the value comes from (this build's
//! own, the admin's, or node.toml), what the service limits in plain words, and the fields and
//! route for using the admin's own app instead. Never a secret, nor the ids that are kept private.
//!
//! The admin's own TIDAL, Jamendo, Google Drive and OneDrive apps are kept in [`OWN_KEY`] (the
//! other services keep theirs under their own keys). node.toml or the environment wins over
//! them; they win over this build's own app (TASK-service-apps-builtin.md).

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// The admin's own apps, `{service id: {client_id, client_secret}}`, for TIDAL, Jamendo, Google
/// Drive and OneDrive. A secret setting: sealed in the encrypted backup, never in `settings.json`.
pub const OWN_KEY: &str = "apps.own";

/// The services whose own app is kept in [`OWN_KEY`].
const OWN: [&str; 4] = ["tidal", "jamendo", "google-drive", "onedrive"];

/// One node app.
#[derive(Debug, Serialize, PartialEq)]
pub struct NodeApp {
    pub id: &'static str,
    pub name: &'static str,
    pub configured: bool,
    /// `settings` (the admin's own, entered here), `config` (node.toml or the environment),
    /// `builtin` (this build's own), or none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// This build carries the project's own app for the service.
    pub builtin: bool,
    /// The route that sets the admin's own app, or none when it can only be set in node.toml.
    /// It takes `fields` by name; the first field empty goes back to the built-in app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set: Option<&'static str>,
    /// What the admin's own app takes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
    /// The redirect addresses an own app lists, for the services that sign in through one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<String>,
    /// The route that says whether it works, where there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    /// Where to get an own app (the "Advanced: use your own app" part of the screen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get_one: Option<&'static str>,
    /// What the service limits, in plain words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

/// One value of an own app.
#[derive(Debug, Serialize, PartialEq)]
pub struct Field {
    pub name: &'static str,
    pub label: &'static str,
    /// Typed into a password box, and never answered back.
    pub secret: bool,
    pub optional: bool,
}

const fn field(name: &'static str, label: &'static str) -> Field {
    Field {
        name,
        label,
        secret: false,
        optional: false,
    }
}

const fn secret(name: &'static str, label: &'static str, optional: bool) -> Field {
    Field {
        name,
        label,
        secret: true,
        optional,
    }
}

fn app(id: &'static str, name: &'static str, configured: bool, from: Option<String>) -> NodeApp {
    NodeApp {
        id,
        name,
        configured,
        from: if configured { from } else { None },
        builtin: false,
        set: None,
        fields: Vec::new(),
        redirects: Vec::new(),
        status: None,
        get_one: None,
        limits: None,
        note: None,
    }
}

/// The admin's own app for one service.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnApp {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
}

impl std::fmt::Debug for OwnApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnApp")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<set>")
            .finish()
    }
}

fn own_apps(lib: &pm_library::Library) -> HashMap<String, OwnApp> {
    lib.get_setting(OWN_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

/// This build's own app for a service in [`OWN`].
fn builtin(id: &str) -> Option<OwnApp> {
    let client_id = match id {
        "tidal" => crate::project_apps::tidal_client_id(),
        "jamendo" => crate::project_apps::jamendo_client_id(),
        "google-drive" => crate::project_apps::google_client_id(),
        "onedrive" => crate::project_apps::onedrive_client_id(),
        _ => None,
    }?;
    Some(OwnApp {
        client_id: client_id.to_string(),
        client_secret: String::new(),
    })
}

fn drive(id: &str) -> Option<crate::drives::Provider> {
    match id {
        "google-drive" => Some(crate::drives::Provider::Google),
        "onedrive" => Some(crate::drives::Provider::Onedrive),
        _ => None,
    }
}

/// The app a service in [`OWN`] signs in with now.
fn current(s: &NodeState, id: &str) -> OwnApp {
    if let Some(p) = drive(id) {
        let d = s.drives.read().expect("drives");
        return d
            .apps
            .get(&p)
            .map(|a| OwnApp {
                client_id: a.client_id.clone(),
                client_secret: a.client_secret.clone(),
            })
            .unwrap_or_default();
    }
    match id {
        "tidal" => {
            let c = s.tidal.read().expect("tidal").creds.clone();
            OwnApp {
                client_id: c.client_id,
                client_secret: c.client_secret,
            }
        }
        "jamendo" => {
            let c = s.jamendo.read().expect("jamendo").creds.clone();
            OwnApp {
                client_id: c.client_id,
                client_secret: c.client_secret,
            }
        }
        _ => OwnApp::default(),
    }
}

/// Where the app a service in [`OWN`] signs in with comes from.
fn origin(s: &NodeState, own: &HashMap<String, OwnApp>, id: &str) -> Option<&'static str> {
    let now = current(s, id);
    if now.client_id.trim().is_empty() {
        None
    } else if own.get(id).is_some_and(|o| o.client_id == now.client_id) {
        Some("settings")
    } else if builtin(id).is_some_and(|b| b.client_id == now.client_id) {
        Some("builtin")
    } else {
        Some("config")
    }
}

/// Signs in with `app` from now on (none: no app).
fn set_app(s: &NodeState, id: &str, app: Option<OwnApp>) {
    let app = app.unwrap_or_default();
    if let Some(p) = drive(id) {
        let mut d = s.drives.write().expect("drives");
        if app.client_id.is_empty() {
            d.apps.remove(&p);
        } else {
            d.apps.insert(
                p,
                crate::drives::App {
                    client_id: app.client_id,
                    client_secret: app.client_secret,
                },
            );
        }
    } else if id == "tidal" {
        let mut t = s.tidal.write().expect("tidal");
        t.creds.client_id = app.client_id;
        t.creds.client_secret = app.client_secret;
    } else if id == "jamendo" {
        let mut j = s.jamendo.write().expect("jamendo");
        j.creds.client_id = app.client_id;
        j.creds.client_secret = app.client_secret;
    }
}

/// At start: the admin's own apps, where node.toml or the environment sets none.
pub fn apply_own(s: &NodeState) {
    let own = own_apps(&s.library.lock().expect("library poisoned"));
    for id in OWN {
        if let Some(app) = own.get(id) {
            if origin(s, &own, id) != Some("config") {
                set_app(s, id, Some(app.clone()));
            }
        }
    }
}

/// Every node app, as `GET /v1/node/apps` lists it. `base` is the node's address as the browser
/// sees it (for the redirect addresses an own app lists).
pub fn list(s: &NodeState, base: &str) -> Vec<NodeApp> {
    // A value stored before origins were recorded has none: it was entered in Settings
    let origin_of = |f: String| if f.is_empty() { "settings".to_string() } else { f };
    let (qobuz, soundcloud, lastfm, tidal_full, acoustid_own, own) = {
        let lib = s.library.lock().expect("library poisoned");
        (
            crate::qobuz::app_of(&lib).map(|a| origin_of(a.from)),
            crate::soundcloud::client_of(&lib).map(|c| origin_of(c.from)),
            crate::lastfm::app_of(&lib).map(|a| origin_of(a.from)),
            crate::tidal_unofficial::client_of(&lib).map(|c| origin_of(c.from)),
            lib.get_setting(crate::identify::KEY_SETTING)
                .ok()
                .flatten()
                .is_some_and(|k| !k.trim().is_empty()),
            own_apps(&lib),
        )
    };
    // The build's own app where the value is the project's, else the node's config
    let built_or_config = |id: &str, built: Option<&str>| {
        if built == Some(id) {
            "builtin".to_string()
        } else {
            "config".to_string()
        }
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
    // An own app in OWN: settable here unless node.toml sets it
    let own_app = |id: &'static str, name: &'static str, fields: Vec<Field>, redirects: Vec<String>| {
        let from = origin(s, &own, id);
        NodeApp {
            builtin: builtin(id).is_some(),
            set: (from != Some("config")).then_some(match id {
                "tidal" => "PUT /v1/node/apps/tidal",
                "jamendo" => "PUT /v1/node/apps/jamendo",
                "google-drive" => "PUT /v1/node/apps/google-drive",
                _ => "PUT /v1/node/apps/onedrive",
            }),
            fields,
            redirects,
            ..app(id, name, from.is_some(), from.map(str::to_string))
        }
    };
    let return_page = s.return_page();
    let spotify = crate::spotify::app_origin(s);
    let mut out =
        vec![
        NodeApp {
            // node.toml wins over Settings: the route refuses then
            set: (spotify != Some("config")).then_some("PUT /v1/sources/spotify/app"),
            fields: vec![field("client_id", "Client ID")],
            redirects: vec![return_page.clone()],
            status: Some("GET /v1/sources/spotify/status"),
            get_one: Some("https://developer.spotify.com/dashboard"),
            limits: Some(
                "Spotify doesn't let one app serve every node: an app in development mode lets in 25 \
                 people, each added by hand under User Management, and its owner needs Premium. More \
                 people need Spotify's approval, which it gives only to large organisations. Playing \
                 through Spotify Connect (librespot) needs no app.",
            ),
            ..app("spotify", "Spotify", spotify.is_some(), spotify.map(str::to_string))
        },
        NodeApp {
            limits: Some(
                "TIDAL gives apps like this one 30-second previews. Whole songs need TIDAL full tracks.",
            ),
            status: Some("GET /v1/sources/tidal/status"),
            get_one: Some("https://developer.tidal.com"),
            ..own_app(
                "tidal",
                "TIDAL",
                vec![
                    field("client_id", "Client ID"),
                    secret("client_secret", "Client secret", true),
                ],
                vec![return_page.clone(), format!("{base}/tidal/callback")],
            )
        },
        NodeApp {
            set: Some("PUT /v1/sources/tidal/unofficial"),
            fields: vec![
                field("client_id", "Device client ID"),
                secret("client_secret", "Device client secret", false),
            ],
            status: Some("GET /v1/sources/tidal/unofficial"),
            limits: Some(
                "Whole songs need a device client, the kind TIDAL's own apps use. TIDAL gives those only \
                 to its partners and can stop accepting one at any time; an app from TIDAL's developer \
                 site doesn't work here.",
            ),
            builtin: crate::project_apps::tidal_device_client().is_some(),
            ..app(
                "tidal-full",
                "TIDAL full tracks",
                tidal_full.is_some(),
                tidal_full,
            )
        },
        NodeApp {
            set: Some("PUT /v1/sources/qobuz/unofficial"),
            fields: vec![field("app_id", "App ID"), secret("app_secret", "App secret", false)],
            status: Some("GET /v1/sources/qobuz/unofficial"),
            limits: Some(
                "Qobuz gives app IDs only to its partners, not to other players, so Pubky Music has none \
                 to build in. Without one, Qobuz doesn't play here.",
            ),
            ..app("qobuz", "Qobuz", qobuz.is_some(), qobuz)
        },
        NodeApp {
            set: Some("PUT /v1/sources/soundcloud/unofficial"),
            fields: vec![field("client_id", "Client ID")],
            status: Some("GET /v1/sources/soundcloud/unofficial"),
            limits: Some("SoundCloud approves each app by hand."),
            ..app("soundcloud", "SoundCloud", soundcloud.is_some(), soundcloud)
        },
        NodeApp {
            set: Some("PUT /v1/sources/youtube/key"),
            fields: vec![secret("key", "API key", false)],
            status: Some("GET /v1/sources/youtube/key"),
            get_one: Some("https://console.cloud.google.com/apis/library/youtube.googleapis.com"),
            limits: Some(
                "Not needed with yt-dlp switched on (Sources › YouTube): it searches too. A key of your own searches \
                 YouTube's Music category, about 100 searches a day; it is checked with YouTube when \
                 it is saved.",
            ),
            ..app(
                "youtube",
                "YouTube Data",
                crate::youtube::has_key(s),
                Some("settings".into()),
            )
        },
        NodeApp {
            set: Some("PUT /v1/lastfm"),
            fields: vec![
                field("api_key", "API key"),
                secret("api_secret", "Shared secret", false),
            ],
            status: Some("GET /v1/lastfm"),
            get_one: Some("https://www.last.fm/api/account/create"),
            ..app("lastfm", "Last.fm", lastfm.is_some(), lastfm)
        },
        NodeApp {
            status: Some("GET /v1/sources/jamendo/status"),
            get_one: Some("https://devportal.jamendo.com"),
            limits: Some(
                "Playing needs only the built-in app. Signing in to your Jamendo favourites needs an \
                 app with its secret.",
            ),
            ..own_app(
                "jamendo",
                "Jamendo",
                vec![
                    field("client_id", "Client ID"),
                    secret("client_secret", "Client secret", true),
                ],
                vec![format!("{base}/jamendo/callback")],
            )
        },
        NodeApp {
            set: Some("PUT /v1/library/identify/settings"),
            fields: vec![secret("acoustid_key", "Application key", false)],
            status: Some("GET /v1/library/identify"),
            get_one: Some("https://acoustid.org/new-application"),
            builtin: crate::project_apps::acoustid_key().is_some(),
            ..app(
                "acoustid",
                "AcoustID",
                acoustid_from.is_some(),
                acoustid_from,
            )
        },
        NodeApp {
            status: Some("GET /v1/library/drives"),
            limits: Some(
                "Until Google verifies an app, at most 100 people can sign in with it, and Google warns \
                 them that the app isn't verified.",
            ),
            ..own_app(
                "google-drive",
                "Google Drive",
                vec![
                    field("client_id", "Client ID"),
                    secret("client_secret", "Client secret", true),
                ],
                vec![return_page.clone(), format!("{base}/drives/google/callback")],
            )
        },
        NodeApp {
            status: Some("GET /v1/library/drives"),
            ..own_app(
                "onedrive",
                "OneDrive",
                vec![field("client_id", "Application (client) ID")],
                vec![return_page],
            )
        },
        NodeApp {
            note: Some("Artist images"),
            builtin: crate::project_apps::fanart_key().is_some(),
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
        note: Some("Artist biographies"),
        limits: (!crate::project_apps::audiodb_built_in()).then_some(
            "Without this build's key, TheAudioDB's public test key allows a few requests a minute.",
        ),
        builtin: true,
        ..app("audiodb", "TheAudioDB", true, Some("builtin".into()))
    });
    out
}

/// `GET /v1/node/apps` (owner): every app the node uses, set once for everyone.
pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Vec<NodeApp>> {
    let base = crate::tidal::external_base(&s, &headers);
    Ok(Json(list(&s, &base)))
}

/// `PUT /v1/node/apps/{id} {client_id, client_secret?}` (owner): the admin's own app for TIDAL,
/// Jamendo, Google Drive or OneDrive; an empty `client_id` goes back to this build's own. Refused
/// while node.toml sets the app. Answers the service's row.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<OwnApp>,
) -> ApiResult<NodeApp> {
    let Some(id) = OWN.iter().copied().find(|o| *o == id) else {
        return Err(ApiError::not_found(format!(
            "{id}: no app of your own is set here (only {})",
            OWN.join(", ")
        )));
    };
    let mut own = own_apps(&s.library.lock().expect("library poisoned"));
    if origin(&s, &own, id) == Some("config") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "This node's app for it is set in node.toml (or the environment); change it there",
        ));
    }
    let client_id = body.client_id.trim().to_string();
    let client_secret = body.client_secret.trim().to_string();
    let next = if client_id.is_empty() {
        None
    } else {
        let plain = |v: &str| v.chars().all(|c| c.is_ascii_graphic());
        if client_id.len() > 256 || !plain(&client_id) {
            return Err(ApiError::bad_request(
                "A client ID is one word of letters, digits and signs: copy it from your app's page",
            ));
        }
        if client_secret.len() > 512 || !plain(&client_secret) {
            return Err(ApiError::bad_request(
                "A client secret is one word of letters, digits and signs: copy it from your app's page",
            ));
        }
        if id == "onedrive" && !client_secret.is_empty() {
            return Err(ApiError::bad_request(
                "OneDrive signs in without a secret: leave it out (a public client)",
            ));
        }
        Some(OwnApp {
            client_id,
            client_secret,
        })
    };
    let back = next.clone().or_else(|| builtin(id));
    let changed = current(&s, id) != back.clone().unwrap_or_default();
    {
        let lib = s.library.lock().expect("library poisoned");
        match &next {
            Some(a) => {
                own.insert(id.to_string(), a.clone());
            }
            None => {
                own.remove(id);
            }
        }
        if own.is_empty() {
            lib.delete_setting(OWN_KEY)?;
        } else {
            lib.set_setting(OWN_KEY, &serde_json::to_string(&own).expect("apps serialize"))?;
        }
    }
    if changed {
        set_app(&s, id, back);
        // A session belongs to the app it was made with
        match id {
            "tidal" => crate::tidal::app_changed(&s)?,
            "jamendo" => crate::jamendo::app_changed(&s)?,
            _ => {}
        }
    }
    let base = crate::tidal::external_base(&s, &headers);
    list(&s, &base)
        .into_iter()
        .find(|a| a.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The admin's own apps travel only in the encrypted backup: never in the settings copy on the
    /// homeserver, never in a `Debug` line.
    #[test]
    fn own_apps_are_kept_as_secrets() {
        assert!(crate::backup::SECRET_SETTINGS.iter().any(|(k, _)| *k == OWN_KEY));
        assert!(crate::sync::is_credential(OWN_KEY));
        let a = OwnApp {
            client_id: "id".into(),
            client_secret: "hush".into(),
        };
        assert!(!format!("{a:?}").contains("hush"));
    }
}
