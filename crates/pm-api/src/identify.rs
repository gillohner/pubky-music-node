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
    /// "Write IDs into my files" (off by default).
    pub write_ids: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IdentifyStart {
    /// Look again at tracks a previous run found nothing for.
    #[serde(default)]
    pub retry: bool,
}

/// `PUT /v1/library/identify/settings`.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentifySettings {
    pub write_ids: bool,
}

/// `POST /v1/library/identify/write`: what was written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WriteReport {
    pub written: usize,
    /// Files left as they were, with the reason (format, read-only share, …).
    pub skipped: Vec<WriteSkip>,
    /// Where the tag backups are.
    pub backup_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteSkip {
    pub track_id: i64,
    pub path: String,
    pub reason: String,
}

const WRITE_IDS_KEY: &str = "library.write_ids";

fn write_ids_on(s: &NodeState) -> bool {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(WRITE_IDS_KEY)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Tag backups live next to the node's other data.
pub(crate) fn backup_dir(s: &NodeState) -> PathBuf {
    s.art_dir
        .parent()
        .map(|p| p.join("tag-backups"))
        .unwrap_or_else(|| PathBuf::from("tag-backups"))
}

/// Write the IDs identification found into the files (all such tracks, or
/// `only`); each file's ID tags are backed up first. Blocking.
pub fn write_found_ids(s: &NodeState, only: Option<&[i64]>) -> Result<WriteReport, ApiError> {
    let found = s
        .library
        .lock()
        .expect("library poisoned")
        .identified_not_tagged()?;
    let dir = backup_dir(s);
    let mut report = WriteReport {
        backup_dir: dir.display().to_string(),
        ..Default::default()
    };
    for (t, release) in found {
        let id = t.id.map(|i| i.0).unwrap_or_default();
        if only.is_some_and(|o| !o.contains(&id)) {
            continue;
        }
        let Some((MusicUri::Recording { mbid }, _)) = t.canonical.clone() else {
            continue;
        };
        let ids = pm_library::tagwrite::Ids {
            recording: mbid,
            release,
            acoustid: t.acoustid.clone(),
        };
        match pm_library::tagwrite::write_ids(&t.file.path, &ids, &dir) {
            Ok(_) => {
                report.written += 1;
                let _ = s.library.lock().expect("library poisoned").set_identify_state(
                    id,
                    "identified",
                    Some("written into the file"),
                );
            }
            Err(e) => report.skipped.push(WriteSkip {
                track_id: id,
                path: t.file.path.display().to_string(),
                reason: e.to_string(),
            }),
        }
    }
    Ok(report)
}

/// `PUT /v1/library/identify/settings {write_ids}`.
pub(crate) async fn put_settings(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<IdentifySettings>,
) -> ApiResult<IdentifyStatus> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(WRITE_IDS_KEY, if body.write_ids { "true" } else { "false" })?;
    Ok(Json(status(&s)?))
}

/// `POST /v1/library/identify/write`: write the IDs found so far into the files now.
pub(crate) async fn write_now(State(s): State<Arc<NodeState>>) -> ApiResult<WriteReport> {
    let st = s.clone();
    let r = tokio::task::spawn_blocking(move || write_found_ids(&st, None))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(r))
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
        write_ids: write_ids_on(s),
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
    let mut identified_now: Vec<i64> = Vec::new();
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
                        release,
                        confidence,
                        acoustid,
                    } => {
                        let lib = s.library.lock().expect("library poisoned");
                        lib.set_identity(
                            id,
                            &MusicUri::Recording { mbid: recording },
                            confidence,
                            Some(&acoustid),
                        )?;
                        lib.set_identify_state(id, "identified", Some(confidence.label()))?;
                        if let Some(r) = release {
                            lib.set_identify_release(id, &r)?;
                        }
                        identified_now.push(id);
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
    // "Write IDs into my files", when the owner turned it on.
    if !identified_now.is_empty() && write_ids_on(s) {
        let st = s.clone();
        let only = identified_now.clone();
        let r = tokio::task::spawn_blocking(move || write_found_ids(&st, Some(&only)))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
        tracing::info!(
            written = r.written,
            skipped = r.skipped.len(),
            "identify: IDs written into files"
        );
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
    if contact.is_empty() || t.title.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut cfg = pm_library::musicbrainz::MusicBrainzConfig::new(contact);
    cfg.base_url = s.mb_base.read().expect("mb lock").clone();
    let mb = pm_library::musicbrainz::MusicBrainzClient::new(cfg).map_err(|e| e.to_string())?;
    let found = mb
        .search_recording_hits(&artist, &t.title, t.duration_ms)
        .await
        .map_err(|e| e.to_string())?;
    // A length far from the file's lowers the score: the owner decides
    let near = |h: &pm_library::musicbrainz::RecordingHit| match (h.length_ms, t.duration_ms) {
        (Some(a), Some(b)) => a.abs_diff(b) <= 3000,
        _ => true,
    };
    // One choice per title, artist and release: identical-looking rows only confuse
    let mut seen = std::collections::HashSet::new();
    Ok(found
        .into_iter()
        .filter(|h| h.score >= 80)
        .filter(|h| seen.insert((h.title.to_lowercase(), h.artist.to_lowercase(), h.release.clone())))
        .take(5)
        .map(|h| Candidate {
            recording: h.mbid,
            release: h.release_mbid,
            title: Some(h.title.clone()),
            artist: Some(h.artist.clone()),
            release_title: h.release.clone(),
            score: h.score as f32 / 100.0 * if near(&h) { 1.0 } else { 0.7 },
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

/// "Which song is this?" for one track: what MusicBrainz suggests, searched now.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackCandidates {
    pub track: Track,
    pub suggestions: Vec<pm_library::SuggestionRow>,
    /// MusicBrainz is asked only with a contact (Settings, or Get started on Home).
    pub needs_contact: bool,
}

/// `GET /v1/library/tracks/{id}/candidates`: search MusicBrainz by the track's tags now (with
/// and, failing that, without its length) and list the suggestions to pick from; picking one is
/// `POST /v1/library/suggestions/{id}/accept`.
pub(crate) async fn track_candidates(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
) -> ApiResult<TrackCandidates> {
    let track = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(pm_model::TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    let needs_contact = s.contact().trim().is_empty();
    if !needs_contact {
        let found = search_suggestions(&s, &track)
            .await
            .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, format!("MusicBrainz: {e}")))?;
        let lib = s.library.lock().expect("library poisoned");
        lib.add_suggestions(id, &found)?;
        if !found.is_empty() {
            lib.set_identify_state(id, "suggested", None)?;
        }
    }
    let suggestions = s.library.lock().expect("library poisoned").suggestions_for(id)?;
    Ok(Json(TrackCandidates {
        track,
        suggestions,
        needs_contact,
    }))
}

/// `POST /v1/library/suggestions/{id}/accept`: the owner confirms; the
/// track's identity becomes that recording (`confirmed_by_user`).
pub(crate) async fn accept(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Track> {
    let (track, t) = {
        let lib = s.library.lock().expect("library poisoned");
        let Some((track, _)) = lib.accept_suggestion(id)? else {
            return Err(ApiError::not_found(format!("no suggestion {id}")));
        };
        let t = lib
            .get_track(pm_model::TrackId(track))?
            .ok_or_else(|| ApiError::not_found("track gone"))?;
        (track, t)
    };
    if write_ids_on(&s) {
        let st = s.clone();
        let _ = tokio::task::spawn_blocking(move || write_found_ids(&st, Some(&[track]))).await;
    }
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
