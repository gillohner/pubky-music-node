//! The "Identify library" job (steward identify brief): tracks without a
//! canonical identity are fingerprinted (Chromaprint, cached by file key),
//! looked up on AcoustID (3 requests a second), and decided folder by folder
//! with the folder as a release. What stays ambiguous becomes suggestions
//! for the owner; with no fingerprint match, a MusicBrainz search by the
//! tags adds suggestions too. Suggestions never become identity on their
//! own. The job resumes: tracks already looked at are skipped unless
//! `retry` is set.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::identify::{best_folder_release, decide, AcoustIdResult, Candidate, Decision, FileFacts};
use pm_library::{IdentityCounts, Library, SuggestionRow};
use pm_model::{MusicUri, Track};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct AcoustIdSettings {
    pub api_key: String,
    pub base_url: String,
}

/// Where the current (or last) run stands.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IdentifyProgress {
    pub running: bool,
    pub total: usize,
    pub done: usize,
    pub identified: usize,
    pub suggested: usize,
    pub unknown: usize,
    pub errors: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

/// `GET /v1/library/identify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifyStatus {
    pub progress: IdentifyProgress,
    pub counts: IdentityCounts,
    /// Whether an AcoustID key is configured (fingerprint lookups).
    pub acoustid: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IdentifyStart {
    /// Look again at tracks a previous run found nothing for.
    #[serde(default)]
    pub retry: bool,
}

/// One suggestion with the track it is for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionView {
    #[serde(flatten)]
    pub row: SuggestionRow,
    pub track: Track,
}

fn status(s: &NodeState) -> Result<IdentifyStatus, ApiError> {
    let counts = s.library.lock().expect("library poisoned").identity_counts()?;
    Ok(IdentifyStatus {
        progress: s.identify.lock().expect("identify lock").clone(),
        counts,
        acoustid: !s.acoustid.read().expect("acoustid lock").api_key.is_empty(),
    })
}

pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<IdentifyStatus> {
    Ok(Json(status(&s)?))
}

/// `POST /v1/library/identify {retry?}`: start the job (no-op while running).
pub(crate) async fn start(
    State(s): State<Arc<NodeState>>,
    body: Option<Json<IdentifyStart>>,
) -> ApiResult<IdentifyStatus> {
    let retry = body.map(|b| b.retry).unwrap_or(false);
    {
        let mut p = s.identify.lock().expect("identify lock");
        if p.running {
            drop(p);
            return Ok(Json(status(&s)?));
        }
        *p = IdentifyProgress {
            running: true,
            started_at: Some(pm_library::db::now_unix()),
            ..Default::default()
        };
    }
    let st = s.clone();
    tokio::spawn(async move {
        if let Err(e) = run(&st, retry).await {
            tracing::warn!("identify: {}", e.message);
            st.identify.lock().expect("identify lock").last_error = Some(e.message);
        }
        let mut p = st.identify.lock().expect("identify lock");
        p.running = false;
        p.finished_at = Some(pm_library::db::now_unix());
        let _ = st.events.send(crate::types::ApiEvent::LibraryChanged);
    });
    Ok(Json(status(&s)?))
}

/// Run identification to the end. Public for tests and the CLI.
pub async fn run(s: &Arc<NodeState>, retry: bool) -> Result<(), ApiError> {
    let todo = s
        .library
        .lock()
        .expect("library poisoned")
        .tracks_to_identify(retry)?;
    s.identify.lock().expect("identify lock").total = todo.len();
    let mut by_folder: BTreeMap<PathBuf, Vec<Track>> = BTreeMap::new();
    for t in todo {
        let dir = t.file.path.parent().map(PathBuf::from).unwrap_or_default();
        by_folder.entry(dir).or_default().push(t);
    }
    let settings = s.acoustid.read().expect("acoustid lock").clone();
    let client = if settings.api_key.is_empty() {
        None
    } else {
        let mut cfg = pm_library::identify::AcoustIdConfig::new(settings.api_key.clone(), s.user_agent());
        cfg.base_url = settings.base_url.clone();
        Some(pm_library::identify::AcoustIdClient::new(cfg).map_err(ApiError::from)?)
    };
    for (dir, tracks) in by_folder {
        let folder_len = s
            .library
            .lock()
            .expect("library poisoned")
            .tracks_in_folder(&dir)
            .map(|v| v.len())
            .unwrap_or(tracks.len());
        let mut looked: Vec<(Track, FileFacts, Result<Vec<AcoustIdResult>, String>)> = Vec::new();
        for t in tracks {
            let facts = FileFacts {
                track_no: t.track_no,
                disc_no: t.disc_no,
                duration_secs: t.duration_ms.or(t.file.duration_ms).map(|ms| ms as f64 / 1000.0),
            };
            let r = lookup(s, client.as_ref(), &t).await;
            looked.push((t, facts, r));
        }
        let pairs: Vec<(FileFacts, &[AcoustIdResult])> = looked
            .iter()
            .filter_map(|(_, f, r)| r.as_ref().ok().map(|r| (*f, r.as_slice())))
            .collect();
        let preferred = best_folder_release(&pairs, folder_len);
        for (t, facts, r) in &looked {
            let id = t.id.map(|i| i.0).unwrap_or_default();
            let outcome = match r {
                Err(e) => Outcome::Error(e.clone()),
                Ok(results) => match decide(results, facts, folder_len, preferred.as_deref()) {
                    Decision::Identified {
                        recording,
                        confidence,
                        acoustid,
                        ..
                    } => {
                        let lib = s.library.lock().expect("library poisoned");
                        lib.set_identity(
                            id,
                            &MusicUri::Recording { mbid: recording },
                            confidence,
                            Some(&acoustid),
                        )?;
                        lib.set_identify_state(id, "identified", Some(confidence.label()))?;
                        Outcome::Identified
                    }
                    Decision::Suggest(c) => Outcome::Suggest(c),
                    Decision::Unknown => Outcome::Unknown,
                },
            };
            let outcome = match outcome {
                // Nothing from the fingerprint: ask MusicBrainz by the tags.
                Outcome::Unknown | Outcome::Error(_) => match search_suggestions(s, t).await {
                    Ok(c) if !c.is_empty() => Outcome::Suggest(c),
                    _ => outcome,
                },
                o => o,
            };
            let mut p = s.identify.lock().expect("identify lock");
            p.done += 1;
            let lib = s.library.lock().expect("library poisoned");
            match outcome {
                Outcome::Identified => p.identified += 1,
                Outcome::Suggest(c) => {
                    lib.add_suggestions(id, &c)?;
                    lib.set_identify_state(id, "suggested", None)?;
                    p.suggested += 1;
                }
                Outcome::Unknown => {
                    lib.set_identify_state(id, "unknown", None)?;
                    p.unknown += 1;
                }
                Outcome::Error(e) => {
                    lib.set_identify_state(id, "error", Some(&e))?;
                    p.errors += 1;
                    p.last_error = Some(e);
                }
            }
        }
    }
    Ok(())
}

enum Outcome {
    Identified,
    Suggest(Vec<Candidate>),
    Unknown,
    Error(String),
}

/// Fingerprint (cached by file key) and look up (cached answer, else
/// AcoustID when a key is set; without one, no results).
async fn lookup(
    s: &NodeState,
    client: Option<&pm_library::identify::AcoustIdClient>,
    t: &Track,
) -> Result<Vec<AcoustIdResult>, String> {
    let path = t.file.path.clone();
    let key = tokio::task::spawn_blocking({
        let p = path.clone();
        move || Library::file_key(&p)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let cached = s
        .library
        .lock()
        .expect("library poisoned")
        .cached_fingerprint(&key)
        .map_err(|e| e.to_string())?;
    let (duration, fp, json) = match cached {
        Some(c) => c,
        None => {
            if t.file.format.kind == pm_model::SampleKind::Dsd {
                return Err("DSD files are not fingerprinted".into());
            }
            let p = path.clone();
            let f = tokio::task::spawn_blocking(move || pm_audio::fingerprint::fingerprint_file(&p))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("fingerprint: {e}"))?;
            s.library
                .lock()
                .expect("library poisoned")
                .save_fingerprint(&key, f.duration_secs, &f.fingerprint)
                .map_err(|e| e.to_string())?;
            (f.duration_secs, f.fingerprint, None)
        }
    };
    if let Some(j) = json {
        let r: pm_library::identify::LookupResponse = serde_json::from_str(&j).map_err(|e| e.to_string())?;
        return Ok(r.results);
    }
    let Some(client) = client else {
        return Ok(Vec::new());
    };
    let (raw, results) = client.lookup(&fp, duration).await.map_err(|e| e.to_string())?;
    s.library
        .lock()
        .expect("library poisoned")
        .save_lookup(&key, &raw)
        .map_err(|e| e.to_string())?;
    Ok(results)
}

/// MusicBrainz recordings matching the tags' artist and title (± 2 s):
/// suggestions only, scored by MusicBrainz's own ranking.
async fn search_suggestions(s: &NodeState, t: &Track) -> Result<Vec<Candidate>, String> {
    let contact = s.contact();
    let artist = t.artists.first().map(|a| a.name.clone()).unwrap_or_default();
    if contact.is_empty() || artist.trim().is_empty() || t.title.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut cfg = pm_library::musicbrainz::MusicBrainzConfig::new(contact);
    cfg.base_url = s.mb_base.read().expect("mb lock").clone();
    let mb = pm_library::musicbrainz::MusicBrainzClient::new(cfg).map_err(|e| e.to_string())?;
    let found = mb
        .search_recording(&artist, &t.title, t.duration_ms)
        .await
        .map_err(|e| e.to_string())?;
    Ok(found
        .into_iter()
        .filter(|(_, score)| *score >= 80)
        .take(3)
        .map(|(mbid, score)| Candidate {
            recording: mbid,
            release: None,
            title: Some(t.title.clone()),
            artist: Some(artist.clone()),
            release_title: t.album.as_ref().map(|a| a.title.clone()),
            score: score as f32 / 100.0,
            source: "search".into(),
        })
        .collect())
}

/// `GET /v1/library/suggestions`: open suggestions with their tracks.
pub(crate) async fn suggestions(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SuggestionView>> {
    let lib = s.library.lock().expect("library poisoned");
    let rows = lib.suggestions()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(track) = lib.get_track(pm_model::TrackId(row.track_id))? {
            out.push(SuggestionView { row, track });
        }
    }
    Ok(Json(out))
}

/// `POST /v1/library/suggestions/{id}/accept`: the owner confirms; the
/// track's identity becomes that recording (`confirmed_by_user`).
pub(crate) async fn accept(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Track> {
    let lib = s.library.lock().expect("library poisoned");
    let Some((track, _)) = lib.accept_suggestion(id)? else {
        return Err(ApiError::not_found(format!("no suggestion {id}")));
    };
    let t = lib
        .get_track(pm_model::TrackId(track))?
        .ok_or_else(|| ApiError::not_found("track gone"))?;
    let _ = s.events.send(crate::types::ApiEvent::LibraryChanged);
    Ok(Json(t))
}

/// `DELETE /v1/library/suggestions/{id}`: not this one.
pub(crate) async fn reject(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if s.library
        .lock()
        .expect("library poisoned")
        .reject_suggestion(id)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no suggestion {id}")))
    }
}
