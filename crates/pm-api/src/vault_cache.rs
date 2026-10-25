//! The node's sign-ins as a cache of the vault (STEWARD-VAULT.md item 6).
//!
//! Each source keeps its sign-in in the node's settings as it always has, and the sources read it
//! there unchanged. This module keeps those settings and the person's vault in step:
//! - the first time, a sign-in that exists only on the node is written into the vault (the
//!   migration);
//! - a sign-in that exists only in the vault (made on another device, or by another node) fills
//!   the node's settings;
//! - after that, the newer side wins: a vault revision the node hasn't seen replaces the node's
//!   copy, and a node copy that changed since the last sync is written to the vault.
//!
//! The vault blob carries the node's settings verbatim (`node.settings`), so any node gets back
//! exactly what it had. Where the setting holds an OAuth session, the standard members (`tokens`,
//! `expires_at`, `user`) are filled too, for the person's other clients. Nothing is deleted from
//! the node's settings. A sign-in that was removed from the vault elsewhere ("Sign out everywhere")
//! is moved aside on the node, to `vault.signed_out.<key>`, so the node stops using it.
//!
//! What stays on the machine: librespot's credentials file, Soloist's and yt-dlp's program state.

use crate::state::NodeState;
use pm_pubky::vault::{Vault, VaultStoreError};
use pubky_music_specs::vault::{ServiceUser, SignIn, VaultError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// One sign-in the vault carries: its id there, and the node settings it lives in. The first
/// setting is the sign-in itself (no sign-in without it); the others travel with it (the person's
/// own app keys, their choices for that source).
pub struct Bridge {
    pub service: &'static str,
    pub settings: &'static [&'static str],
}

/// Every sign-in the node keeps for the person, by source.
pub const BRIDGES: &[Bridge] = &[
    Bridge {
        service: "tidal",
        settings: &["tidal.user"],
    },
    Bridge {
        service: "tidal-full",
        settings: &["tidal.unofficial", "tidal.unofficial_client"],
    },
    Bridge {
        service: "spotify",
        settings: &["spotify.user", "spotify.app"],
    },
    Bridge {
        service: "qobuz",
        settings: &["qobuz.unofficial", "qobuz.app"],
    },
    Bridge {
        service: "soundcloud",
        settings: &["soundcloud.unofficial", "soundcloud.client"],
    },
    Bridge {
        service: "bandcamp",
        settings: &["bandcamp.unofficial", "bandcamp.identity"],
    },
    Bridge {
        service: "youtube",
        settings: &["youtube.api_key"],
    },
    Bridge {
        service: "lastfm",
        settings: &["lastfm.session", "lastfm.app"],
    },
    Bridge {
        service: "listenbrainz",
        settings: &["listenbrainz.account"],
    },
    Bridge {
        service: "jamendo",
        settings: &["jamendo.user"],
    },
    Bridge {
        service: "plex",
        settings: &["plex.server", "plex.client_id"],
    },
    Bridge {
        service: "jellyfin",
        settings: &["jellyfin.server"],
    },
    Bridge {
        service: "opensubsonic",
        settings: &["opensubsonic.server"],
    },
    Bridge {
        service: "audiobookshelf",
        settings: &["audiobookshelf.server"],
    },
    Bridge {
        service: "drives",
        settings: &["drives.accounts"],
    },
    Bridge {
        service: "gpodder",
        settings: &["podcasts.gpodder"],
    },
];

/// What the node last synced for a service: the vault revision and the hash of its settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Synced {
    revision: u64,
    hash: String,
}

fn synced_key(service: &str) -> String {
    format!("vault.synced.{service}")
}

/// What one sync did, per service.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct CacheReport {
    /// Written to the vault for the first time.
    pub migrated: Vec<String>,
    /// The node's copy changed: written to the vault.
    pub pushed: Vec<String>,
    /// The vault's copy was newer (or new): the node's settings now hold it.
    pub pulled: Vec<String>,
    /// Removed from the vault elsewhere: moved aside on the node.
    pub signed_out: Vec<String>,
    /// In the vault, but this node can't read it (another device must share it).
    pub not_readable: Vec<String>,
}

/// The node's settings for a bridge, verbatim (`None` without the main one).
fn local(s: &NodeState, b: &Bridge) -> Option<Map<String, Value>> {
    let lib = s.library.lock().expect("library poisoned");
    lib.get_setting(b.settings[0]).ok().flatten()?;
    let mut m = Map::new();
    for key in b.settings {
        if let Ok(Some(v)) = lib.get_setting(key) {
            m.insert((*key).to_string(), Value::String(v));
        }
    }
    Some(m)
}

fn hash(m: &Map<String, Value>) -> String {
    blake3::hash(serde_json::to_string(m).unwrap_or_default().as_bytes())
        .to_hex()
        .to_string()
}

/// A setting's value as JSON, when it is a JSON object.
fn as_object(raw: &Value) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(raw.as_str()?).ok()? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// The vault sign-in for the node's settings: verbatim under `node.settings`, and the standard
/// members where the main setting is an OAuth session (at the top, or under `session`).
pub fn sign_in_from(service: &str, settings: &Map<String, Value>, main: &str) -> SignIn {
    let mut s: SignIn = serde_json::from_value(json!({ "service": service })).expect("a minimal sign-in");
    s.extra.insert("node".into(), json!({ "settings": settings }));
    let Some(obj) = settings.get(main).and_then(as_object) else {
        return s;
    };
    let sess = match obj.get("session") {
        Some(Value::Object(o)) if o.contains_key("access_token") => o.clone(),
        _ => obj.clone(),
    };
    if let Some(at) = sess.get("access_token").and_then(Value::as_str) {
        s.tokens.insert("access_token".into(), json!(at));
        s.tokens.insert("token_type".into(), json!("bearer"));
        if let Some(rt) = sess.get("refresh_token").and_then(Value::as_str) {
            s.tokens.insert("refresh_token".into(), json!(rt));
        }
        s.expires_at = sess
            .get("expires_at")
            .and_then(Value::as_i64)
            .and_then(|e| u64::try_from(e).ok());
    }
    let id = ["user_id", "id", "username"]
        .iter()
        .find_map(|k| match sess.get(*k) {
            Some(Value::String(v)) => Some(v.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        });
    if let Some(id) = id {
        let pick = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| sess.get(*k).and_then(Value::as_str).map(str::to_string))
        };
        s.user = Some(ServiceUser {
            id,
            name: pick(&["display_name", "name", "username"]),
            country: pick(&["country", "country_code"]),
            extra: Map::new(),
        });
    }
    s
}

/// The node settings a vault sign-in carries, if a node wrote it.
fn node_settings(s: &SignIn) -> Option<Map<String, Value>> {
    match s.extra.get("node")?.get("settings")? {
        Value::Object(o) => Some(o.clone()),
        _ => None,
    }
}

fn apply(s: &NodeState, b: &Bridge, settings: &Map<String, Value>) {
    let lib = s.library.lock().expect("library poisoned");
    for key in b.settings {
        if let Some(Value::String(v)) = settings.get(*key) {
            let _ = lib.set_setting(key, v);
        }
    }
}

fn remember(s: &NodeState, service: &str, synced: &Synced) {
    let lib = s.library.lock().expect("library poisoned");
    let _ = lib.set_setting(
        &synced_key(service),
        &serde_json::to_string(synced).unwrap_or_default(),
    );
}

fn last_synced(s: &NodeState, service: &str) -> Option<Synced> {
    let lib = s.library.lock().expect("library poisoned");
    lib.get_setting(&synced_key(service))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
}

/// Removed from the vault elsewhere: the node's copy moves to `vault.signed_out.<key>` (kept,
/// not used), and the sync record goes.
fn move_aside(s: &NodeState, b: &Bridge) {
    let lib = s.library.lock().expect("library poisoned");
    if let Ok(Some(v)) = lib.get_setting(b.settings[0]) {
        let _ = lib.set_setting(&format!("vault.signed_out.{}", b.settings[0]), &v);
        let _ = lib.delete_setting(b.settings[0]);
    }
    let _ = lib.delete_setting(&synced_key(b.service));
}

/// One pass over every bridge (see the module docs).
pub async fn sync(s: &NodeState, vault: &Vault<'_>) -> Result<CacheReport, VaultStoreError> {
    let mut report = CacheReport::default();
    for b in BRIDGES {
        let mine = local(s, b);
        let theirs = match vault.read(b.service).await {
            Ok(o) => o,
            Err(VaultStoreError::Vault(VaultError::NotARecipient)) => {
                report.not_readable.push(b.service.into());
                continue;
            }
            Err(e) => return Err(e),
        };
        let seen = last_synced(s, b.service);
        match (mine, theirs) {
            (None, None) => {}
            (Some(m), None) => {
                if seen.is_some() {
                    // We had synced it, and it's gone from the vault: signed out elsewhere
                    move_aside(s, b);
                    report.signed_out.push(b.service.into());
                } else {
                    let revision = vault
                        .write_shared(b.service, &sign_in_from(b.service, &m, b.settings[0]))
                        .await?;
                    remember(
                        s,
                        b.service,
                        &Synced {
                            revision,
                            hash: hash(&m),
                        },
                    );
                    report.migrated.push(b.service.into());
                }
            }
            (None, Some(o)) => {
                if let Some(settings) = node_settings(&o.sign_in) {
                    apply(s, b, &settings);
                    let m = local(s, b).unwrap_or_default();
                    remember(
                        s,
                        b.service,
                        &Synced {
                            revision: o.sign_in.revision,
                            hash: hash(&m),
                        },
                    );
                    report.pulled.push(b.service.into());
                }
            }
            (Some(m), Some(o)) => {
                let h = hash(&m);
                let seen = seen.unwrap_or_default();
                if o.sign_in.revision > seen.revision {
                    // The vault moved on (another device refreshed or signed in again): it wins
                    match node_settings(&o.sign_in) {
                        Some(settings) => {
                            apply(s, b, &settings);
                            let m = local(s, b).unwrap_or_default();
                            remember(
                                s,
                                b.service,
                                &Synced {
                                    revision: o.sign_in.revision,
                                    hash: hash(&m),
                                },
                            );
                            report.pulled.push(b.service.into());
                        }
                        // Written by a client that isn't a node: keep ours, write it back
                        None if h != seen.hash => {
                            let mut si = sign_in_from(b.service, &m, b.settings[0]);
                            si.settings = o.sign_in.settings.clone();
                            let revision = vault.write_shared(b.service, &si).await?;
                            remember(s, b.service, &Synced { revision, hash: h });
                            report.pushed.push(b.service.into());
                        }
                        None => remember(
                            s,
                            b.service,
                            &Synced {
                                revision: o.sign_in.revision,
                                hash: h,
                            },
                        ),
                    }
                } else if h != seen.hash {
                    let mut si = sign_in_from(b.service, &m, b.settings[0]);
                    // Keep what other clients put in (the quality choice)
                    si.settings = o.sign_in.settings.clone();
                    let revision = vault.write_shared(b.service, &si).await?;
                    remember(s, b.service, &Synced { revision, hash: h });
                    report.pushed.push(b.service.into());
                }
            }
        }
    }
    Ok(report)
}

/// How often the node syncs with the vault while it is signed in.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// After a sign-in or a restored session: list the node in the device list, share with devices
/// that joined, sync the sign-ins; then again every [`EVERY`] while this session is the node's.
pub fn start(s: &Arc<NodeState>, sync: Arc<crate::sync::PubkySync>) {
    let s = s.clone();
    tokio::spawn(async move {
        loop {
            if !s.sync_handle().is_some_and(|h| Arc::ptr_eq(&h, &sync)) {
                break;
            }
            match crate::vault::client_secret(&s) {
                Ok(secret) => {
                    let vault = Vault::new(sync.session(), &secret).device("node");
                    match pass(&s, &vault).await {
                        Ok(r) if r != CacheReport::default() => tracing::info!(?r, "vault: sign-ins synced"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("vault: {e}"),
                    }
                }
                Err(e) => tracing::warn!("vault: {}", e.message),
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Register, share, sync: one round of [`start`].
pub async fn pass(s: &NodeState, vault: &Vault<'_>) -> Result<CacheReport, VaultStoreError> {
    vault.touch(&s.name).await?;
    vault.share_with_trusted().await?;
    sync(s, vault).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
            .collect()
    }

    #[test]
    fn an_oauth_session_fills_the_standard_members() {
        // Spotify's shape: top-level tokens, expires_at in seconds, user fields
        let m = settings(&[
            (
                "spotify.user",
                r#"{"access_token":"at","refresh_token":"rt","expires_at":1761403600,"user_id":"gil","display_name":"Gil","country":"CH","scope":"x"}"#,
            ),
            ("spotify.app", r#"{"client_id":"own-app"}"#),
        ]);
        let s = sign_in_from("spotify", &m, "spotify.user");
        assert_eq!(s.tokens["access_token"], "at");
        assert_eq!(s.tokens["refresh_token"], "rt");
        assert_eq!(s.tokens["token_type"], "bearer");
        assert_eq!(s.expires_at, Some(1761403600));
        let u = s.user.clone().unwrap();
        assert_eq!(
            (u.id.as_str(), u.name.as_deref(), u.country.as_deref()),
            ("gil", Some("Gil"), Some("CH"))
        );
        // Verbatim for nodes, both settings
        assert_eq!(node_settings(&s).unwrap(), m);
        assert!(!format!("{s:?}").contains("\"rt\""), "Debug shows a token");
    }

    #[test]
    fn a_nested_session_and_plain_values() {
        // The unofficial modes keep the session under `session`
        let m = settings(&[(
            "tidal.unofficial",
            r#"{"enabled":true,"quality":"max","session":{"access_token":"a2","refresh_token":"r2","expires_at":5,"user_id":42}}"#,
        )]);
        let s = sign_in_from("tidal-full", &m, "tidal.unofficial");
        assert_eq!(s.tokens["access_token"], "a2");
        assert_eq!(s.user.unwrap().id, "42");
        // An API key is a plain string: carried verbatim, no standard members
        let m = settings(&[("youtube.api_key", "AIza-not-real")]);
        let s = sign_in_from("youtube", &m, "youtube.api_key");
        assert!(s.tokens.is_empty() && s.user.is_none());
        assert_eq!(node_settings(&s).unwrap(), m);
    }

    #[test]
    fn a_bridge_for_a_source_uses_its_source_id() {
        let sources: Vec<&str> = crate::streaming::services().iter().map(|s| s.id()).collect();
        // Sign-ins of sources are named by the source's id (VAULT.md); the rest are integrations
        for b in BRIDGES {
            let base = b.service.split('-').next().unwrap_or(b.service);
            let integration = ["lastfm", "listenbrainz", "drives", "gpodder"].contains(&b.service);
            assert!(
                integration || sources.contains(&base),
                "{} is not a source id",
                b.service
            );
        }
    }

    #[test]
    fn bridges_are_valid_vault_ids_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in BRIDGES {
            assert!(
                pubky_music_specs::vault::is_service_id(b.service),
                "{}",
                b.service
            );
            assert!(seen.insert(b.service), "{} twice", b.service);
            assert!(!b.settings.is_empty());
        }
        // What stays on the machine
        for local in ["spotify.librespot", "spotify.soloist", "youtube.unofficial"] {
            assert!(BRIDGES.iter().all(|b| !b.settings.contains(&local)), "{local}");
        }
    }
}
