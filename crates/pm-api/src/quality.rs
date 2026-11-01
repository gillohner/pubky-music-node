//! One front for every source's quality choice (STEWARD-UX-PLAYER-SOURCES.md §2): what can be
//! chosen, what is chosen, and when a change applies. The sources' own status answers keep their
//! `quality` fields; this is what the player's quality sheet reads and writes, and what the vault's
//! `settings.quality` will carry across clients. Sources that send one format say so in a note.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{state::NodeState, ApiError, ApiResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `GET /v1/sources/{source}/quality`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quality {
    pub source: String,
    /// Best first; empty when the service sends one format.
    pub options: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// When a change takes effect: `next_track`, or `now` (the stream restarts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies: Option<String>,
    /// What there is to know: why there is no choice, or what limits the choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn choice(id: &str, label: &str) -> Choice {
    Choice {
        id: id.into(),
        label: label.into(),
        note: None,
    }
}

fn one_format(source: &str, note: &str) -> Quality {
    Quality {
        source: source.into(),
        options: Vec::new(),
        current: None,
        applies: None,
        note: Some(note.into()),
    }
}

pub(crate) fn describe(s: &NodeState, source: &str) -> Result<Quality, ApiError> {
    crate::streaming::find(source)?;
    Ok(match source {
        crate::tidal::SOURCE_ID => {
            let st = crate::tidal_unofficial::load(&s.library.lock().expect("library poisoned"));
            Quality {
                source: source.into(),
                options: vec![
                    choice("max", "Max (up to 24-bit/192 kHz)"),
                    choice("high", "High (16-bit/44.1 kHz)"),
                ],
                current: Some(if st.quality == "high" { "high" } else { "max" }.into()),
                applies: Some("next_track".into()),
                note: (!(st.enabled && st.session.is_some())).then(|| {
                    "Applies to full tracks through the device sign-in (a device client of your own); the official app plays 30-second previews on TIDAL's third-party tier.".into()
                }),
            }
        }
        "qobuz" => {
            let st = crate::qobuz::load(&s.library.lock().expect("library poisoned"));
            let cd_only = st.session.as_ref().is_some_and(|x| !x.hires);
            Quality {
                source: source.into(),
                options: vec![
                    choice("max", "Max (up to 24-bit/192 kHz)"),
                    choice("cd", "CD (16-bit/44.1 kHz)"),
                    choice("mp3", "MP3 (320 kbps)"),
                ],
                current: Some(
                    match st.quality.as_str() {
                        "cd" => "cd",
                        "mp3" => "mp3",
                        _ => "max",
                    }
                    .into(),
                ),
                applies: Some("next_track".into()),
                note: cd_only.then(|| "This Qobuz plan streams up to CD quality; Max plays as CD.".into()),
            }
        }
        "spotify" => {
            let st = crate::librespot::load(&s.library.lock().expect("library poisoned"));
            Quality {
                source: source.into(),
                options: vec![
                    choice("320", "320 kbps (Ogg Vorbis)"),
                    choice("160", "160 kbps (Ogg Vorbis)"),
                    choice("96", "96 kbps (Ogg Vorbis)"),
                ],
                current: Some(crate::librespot::bitrate_of(&st).into()),
                applies: Some("now".into()),
                note: Some("librespot's stream into the node; it restarts on a change. Playback on your Spotify devices through Connect is set in the Spotify app.".into()),
            }
        }
        "deezer" => one_format(source, "30-second previews only: Deezer sends one format."),
        "youtube" => one_format(
            source,
            "YouTube picks the stream: AAC up to 128 kbps, in This browser and through yt-dlp.",
        ),
        "bandcamp" => one_format(
            source,
            "Streams are MP3 128 kbps; the download format for purchases is set on the Bandcamp tab.",
        ),
        _ => one_format(source, "The service sends one format; nothing to choose."),
    })
}

pub(crate) async fn get(State(s): State<Arc<NodeState>>, Path(source): Path<String>) -> ApiResult<Quality> {
    Ok(Json(describe(&s, &source)?))
}

#[derive(Debug, Deserialize)]
pub struct Pick {
    pub id: String,
}

/// `PUT /v1/sources/{source}/quality {id}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    Json(p): Json<Pick>,
) -> ApiResult<Quality> {
    let q = describe(&s, &source)?;
    if q.options.is_empty() {
        return Err(ApiError::bad_request(format!(
            "{source} has no quality choice: {}",
            q.note.unwrap_or_default()
        )));
    }
    if !q.options.iter().any(|o| o.id == p.id) {
        let ids: Vec<&str> = q.options.iter().map(|o| o.id.as_str()).collect();
        return Err(ApiError::bad_request(format!(
            "quality is one of {}",
            ids.join(", ")
        )));
    }
    match source.as_str() {
        crate::tidal::SOURCE_ID => {
            crate::tidal_unofficial::update(&s, |st| st.quality = p.id.clone());
        }
        "qobuz" => {
            crate::qobuz::update(&s, |st| st.quality = p.id.clone());
        }
        "spotify" => crate::librespot::set_bitrate(&s, &p.id).await,
        _ => {}
    }
    Ok(Json(describe(&s, &source)?))
}
