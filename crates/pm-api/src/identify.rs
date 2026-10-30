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
    /// The key from the config, env or build; a key entered in Settings (`acoustid.api_key`)
    /// is used instead, and clearing it goes back to this one.
    pub configured: String,
}

/// A key entered in Settings › Library (second review: it needed a config-file edit).
pub const KEY_SETTING: &str = "acoustid.api_key";

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
    /// AcoustID refused the key during this run (why, in its words): nothing more was
    /// fingerprinted. Cleared when the key changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acoustid_refused: Option<String>,
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
    /// Whether that key was entered in Settings (it can be changed or removed there).
    pub acoustid_own: bool,
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
    #[serde(default)]
    pub write_ids: Option<bool>,
    /// Your own AcoustID key; empty removes it.
    #[serde(default)]
    pub acoustid_key: Option<String>,
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
    if let Some(w) = body.write_ids {
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(WRITE_IDS_KEY, if w { "true" } else { "false" })?;
    }
    if let Some(k) = body.acoustid_key {
        let k = k.trim().to_string();
        if k.len() > 64 || k.chars().any(|c| !c.is_ascii_alphanumeric()) {
            return Err(ApiError::bad_request(
                "That doesn't look like an AcoustID key (letters and digits, from acoustid.org/new-application)",
            ));
        }
        // One lookup with the new key: one AcoustID doesn't know is refused here, not found out
        // song by song (an AcoustID out of reach doesn't stop the key being saved)
        if !k.is_empty() {
            let base = s.acoustid.read().expect("acoustid lock").base_url.clone();
            if let Some(why) = acoustid_client(&s, &k, &base)?.key_refused().await {
                return Err(ApiError::bad_request(format!(
                    "AcoustID refused this key ({why}). Copy the API key of your application on acoustid.org, or register one at acoustid.org/new-application"
                )));
            }
        }
        s.identify.lock().expect("identify lock").acoustid_refused = None;
        {
            let lib = s.library.lock().expect("library poisoned");
            if k.is_empty() {
                lib.delete_setting(KEY_SETTING)?;
            } else {
                lib.set_setting(KEY_SETTING, &k)?;
            }
        }
        let mut a = s.acoustid.write().expect("acoustid lock");
        a.api_key = if k.is_empty() { a.configured.clone() } else { k };
    }
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
    let (counts, acoustid_own) = {
        let lib = s.library.lock().expect("library poisoned");
        let own = lib.get_setting(KEY_SETTING)?.is_some_and(|k| !k.is_empty());
        (lib.identity_counts()?, own)
    };
    Ok(IdentifyStatus {
        progress: s.identify.lock().expect("identify lock").clone(),
        counts,
        acoustid: !s.acoustid.read().expect("acoustid lock").api_key.is_empty(),
        acoustid_own,
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
        Some(acoustid_client(s, &settings.api_key, &settings.base_url)?)
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
            // Once AcoustID refused the key, nothing more is fingerprinted (the search by tags
            // still runs)
            let refused = s
                .identify
                .lock()
                .expect("identify lock")
                .acoustid_refused
                .is_some();
            let r = lookup(s, client.as_ref().filter(|_| !refused), &t).await;
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
/// AcoustID when a key is set; without one, or once AcoustID refused it,
/// no results and nothing decoded).
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
    if let Some((_, _, Some(j))) = &cached {
        let r: pm_library::identify::LookupResponse = serde_json::from_str(j).map_err(|e| e.to_string())?;
        return Ok(r.results);
    }
    // Without a key (or one AcoustID refused) a fingerprint answers nothing: not decoded
    let Some(client) = client else {
        return Ok(Vec::new());
    };
    let (duration, fp) = match cached {
        Some((d, fp, _)) => (d, fp),
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
            (f.duration_secs, f.fingerprint)
        }
    };
    let (raw, results) = match client.lookup(&fp, duration).await {
        Ok(r) => r,
        Err(pm_library::LibraryError::AcoustIdKey(why)) => {
            tracing::warn!("identify: AcoustID refused the key ({why}); nothing more is fingerprinted");
            s.identify.lock().expect("identify lock").acoustid_refused = Some(why);
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.to_string()),
    };
    s.library
        .lock()
        .expect("library poisoned")
        .save_lookup(&key, &raw)
        .map_err(|e| e.to_string())?;
    Ok(results)
}

fn acoustid_client(
    s: &NodeState,
    key: &str,
    base_url: &str,
) -> Result<pm_library::identify::AcoustIdClient, ApiError> {
    let mut cfg = pm_library::identify::AcoustIdConfig::new(key, s.user_agent());
    cfg.base_url = base_url.to_string();
    pm_library::identify::AcoustIdClient::new(cfg).map_err(ApiError::from)
}

/// MusicBrainz recordings matching the tags' artist and title (± 2 s): suggestions only, ranked
/// by [`rank_hits`]. A file with no artist gets none: a title alone matches strangers' songs.
async fn search_suggestions(s: &NodeState, t: &Track) -> Result<Vec<Candidate>, String> {
    let contact = s.contact();
    let artist = t.artists.first().map(|a| a.name.clone()).unwrap_or_default();
    if contact.is_empty() || t.title.trim().is_empty() || artist.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut cfg = pm_library::musicbrainz::MusicBrainzConfig::new(contact);
    cfg.base_url = s.mb_base.read().expect("mb lock").clone();
    let mb = pm_library::musicbrainz::MusicBrainzClient::new(cfg).map_err(|e| e.to_string())?;
    let found = mb
        .search_recording_hits(&artist, &t.title, t.duration_ms)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rank_hits(
        &t.title,
        t.album.as_ref().map(|a| a.title.as_str()),
        t.duration_ms,
        found,
    ))
}

/// A title as people compare them: lower case, without "(Live)", "[Remastered]" or
/// " - 2011 Remaster", and with punctuation as spaces.
fn plain_title(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    let s = s.split(" - ").next().unwrap_or(s);
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.extend(if c.is_alphanumeric() {
                c.to_lowercase().collect::<Vec<_>>()
            } else {
                vec![' ']
            }),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether two titles name the same song: equal when plain, or one is the other with a few
/// words more ("Bohemian Rhapsody" and "Bohemian Rhapsody Live Aid").
fn same_title(a: &str, b: &str) -> Option<bool> {
    let (a, b) = (plain_title(a), plain_title(b));
    if a.is_empty() || b.is_empty() {
        return None;
    }
    if a == b {
        return Some(true);
    }
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    (long.starts_with(short.as_str()) && short.len() * 10 >= long.len() * 6).then_some(false)
}

/// Rank MusicBrainz's hits for a file (second review #5, #6):
/// - a hit must carry the file's title (MusicBrainz also returns songs that only mention it);
/// - of the releases a recording is on, the one named like the file's album wins, else the
///   first studio release; bootlegs, live albums and compilations rank lower unless the file's
///   album is one;
/// - a length more than 3 s off, a title with extra words or another album lower the score.
fn rank_hits(
    title: &str,
    album: Option<&str>,
    duration_ms: Option<u64>,
    hits: Vec<pm_library::musicbrainz::RecordingHit>,
) -> Vec<Candidate> {
    let album = album.map(plain_title).filter(|a| !a.is_empty());
    let mut out: Vec<Candidate> = hits
        .into_iter()
        .filter(|h| h.score >= 80)
        .filter_map(|h| {
            let exact = same_title(title, &h.title)?;
            let near = match (h.length_ms, duration_ms) {
                (Some(a), Some(b)) => a.abs_diff(b) <= 3000,
                _ => true,
            };
            let on_album = album
                .as_ref()
                .and_then(|al| h.releases.iter().find(|r| plain_title(&r.title) == *al));
            let pick = on_album
                .or_else(|| h.releases.iter().find(|r| !r.is_secondary()))
                .or(h.releases.first());
            let (release, release_title) = match pick {
                Some(r) => (r.mbid, Some(r.title.clone())),
                None => (h.release_mbid, h.release.clone()),
            };
            let mut score = h.score as f32 / 100.0;
            if !near {
                score *= 0.7;
            }
            if !exact {
                score *= 0.9;
            }
            if album.is_some() && on_album.is_none() {
                score *= 0.9;
            }
            if on_album.is_none() && pick.is_some_and(|r| r.is_secondary()) {
                score *= 0.85;
            }
            Some(Candidate {
                recording: h.mbid,
                release,
                title: Some(h.title.clone()),
                artist: Some(h.artist.clone()),
                release_title,
                score,
                source: "search".into(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    // One choice per title, artist and release: identical-looking rows only confuse
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| {
        seen.insert((
            c.title.as_deref().unwrap_or_default().to_lowercase(),
            c.artist.as_deref().unwrap_or_default().to_lowercase(),
            c.release_title.clone(),
        ))
    });
    out.truncate(5);
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use pm_library::musicbrainz::{HitRelease, RecordingHit};
    use pm_model::Mbid;

    const R1: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const R2: &str = "0383dadf-2a4e-4d10-a46a-e9e041da8eb3";
    const R3: &str = "5b11f4ce-a62d-471e-81fc-a69a8278c7da";

    fn rel(title: &str, id: &str, status: &str, kinds: &[&str]) -> HitRelease {
        HitRelease {
            title: title.into(),
            mbid: Mbid::parse(id).ok(),
            status: status.into(),
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
        }
    }

    fn hit(id: &str, title: &str, length_ms: u64, releases: Vec<HitRelease>) -> RecordingHit {
        RecordingHit {
            mbid: Mbid::parse(id).unwrap(),
            score: 100,
            title: title.into(),
            artist: "Queen".into(),
            release: releases.first().map(|r| r.title.clone()),
            release_mbid: releases.first().and_then(|r| r.mbid),
            length_ms: Some(length_ms),
            releases,
        }
    }

    #[test]
    fn the_files_album_and_studio_releases_come_first() {
        // Second review #5: a live bootleg first, "Greatest Hits II" above "A Night at the Opera"
        let hits = vec![
            hit(
                R1,
                "Death on Two Legs",
                223_000,
                vec![rel("Ultimate Houston 1977", R1, "Bootleg", &["Album", "Live"])],
            ),
            hit(
                R2,
                "Death on Two Legs (Dedicated to…)",
                225_000,
                vec![
                    rel("Greatest Hits II", R2, "Official", &["Album", "Compilation"]),
                    rel("A Night at the Opera", R3, "Official", &["Album"]),
                ],
            ),
        ];
        let c = rank_hits(
            "Death on Two Legs",
            Some("A Night at the Opera"),
            Some(223_000),
            hits,
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].recording, Mbid::parse(R2).unwrap());
        assert_eq!(c[0].release_title.as_deref(), Some("A Night at the Opera"));
        assert_eq!(c[0].release, Mbid::parse(R3).ok());
        assert!(c[1].score < c[0].score);
        assert_eq!(c[1].release_title.as_deref(), Some("Ultimate Houston 1977"));
        // Without an album tag, the studio release is picked over the compilation
        let hits = vec![hit(
            R2,
            "Death on Two Legs",
            223_000,
            vec![
                rel("Greatest Hits II", R2, "Official", &["Album", "Compilation"]),
                rel("A Night at the Opera", R3, "Official", &["Album"]),
            ],
        )];
        let c = rank_hits("Death on Two Legs", None, Some(223_000), hits);
        assert_eq!(c[0].release_title.as_deref(), Some("A Night at the Opera"));
        assert_eq!(c[0].score, 1.0);
    }

    #[test]
    fn a_hit_must_carry_the_files_title() {
        // Second review #6: a long, unrelated title that only mentions the file's
        let hits = vec![
            hit(
                R1,
                "HOLY SHIT EACH EAR HAS ITS OWN TRACK?! (fuck mono audio)",
                5_000,
                vec![],
            ),
            hit(R2, "Mono Audio", 5_000, vec![]),
            hit(R3, "Mono Audio Test Tone For Speakers", 5_000, vec![]),
        ];
        let c = rank_hits("Mono audio", None, Some(5_000), hits);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].recording, Mbid::parse(R2).unwrap());
    }

    #[test]
    fn plain_titles() {
        assert_eq!(
            plain_title("Bohemian Rhapsody - Remastered 2011"),
            "bohemian rhapsody"
        );
        assert_eq!(
            plain_title("Get Lucky (Radio Edit) [feat. Pharrell]"),
            "get lucky"
        );
        assert_eq!(same_title("Don't Stop Me Now", "Don’t Stop Me Now"), Some(true));
        assert_eq!(
            same_title("Bohemian Rhapsody", "Bohemian Rhapsody (Live Aid)"),
            Some(true)
        );
        assert_eq!(same_title("Love", "Love of My Life"), None);
    }
}
