//! "Fix my files' tags" (owner, 2026-10-22: "have for our local files a writer that fixes
//! this"): what the node knows that the files do not say yet, written into them on request.
//!
//! - the artists as the node knows them, one value each (`ARTISTS`, with MusicBrainz artist ids
//!   when known), so other players see "Sefa" and "Dr. Peacock" instead of one artist;
//! - the MusicBrainz recording and release a match found (fingerprint, or the owner's pick).
//!
//! `GET` lists what would change per file; `POST` writes it. Every file's touched tags are saved
//! to a backup first, and the audio is never rewritten (pm_library::tagwrite).

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::Json;
use pm_library::tagwrite::{Fixes, Ids};
use pm_model::{MusicUri, Track};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFix {
    pub track_id: i64,
    pub title: String,
    pub path: String,
    /// `{key, now, then}` per tag that changes.
    pub changes: Vec<TagChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagChange {
    pub key: String,
    pub now: Vec<String>,
    pub then: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixList {
    /// Files with something to fix (all of them; `files` lists the first ones).
    pub total: usize,
    pub files: Vec<FileFix>,
    /// Files the writer cannot change, with why (format, unreadable).
    pub cannot: usize,
}

#[derive(Debug, Deserialize)]
pub struct FixQuery {
    pub limit: Option<usize>,
}

/// The fixes the node would write into each candidate file.
fn candidates(s: &NodeState) -> Result<Vec<(Track, Fixes)>, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let mut by_track: BTreeMap<i64, (Track, Fixes)> = BTreeMap::new();
    for t in lib.tracks_with_several_artists()? {
        let Some(id) = t.id.map(|i| i.0) else { continue };
        let fixes = Fixes {
            ids: None,
            artists: t.artists.iter().map(|a| a.name.clone()).collect(),
            artist_mbids: t.artists.iter().filter_map(|a| a.mbid).collect(),
        };
        by_track.insert(id, (t, fixes));
    }
    for (t, release) in lib.identified_not_tagged()? {
        let Some(id) = t.id.map(|i| i.0) else { continue };
        let Some((MusicUri::Recording { mbid }, _)) = t.canonical.clone() else {
            continue;
        };
        let ids = Ids {
            recording: mbid,
            release,
            acoustid: t.acoustid.clone(),
        };
        by_track
            .entry(id)
            .and_modify(|(_, f)| f.ids = Some(ids.clone()))
            .or_insert_with(|| {
                (
                    t.clone(),
                    Fixes {
                        ids: Some(ids.clone()),
                        ..Default::default()
                    },
                )
            });
    }
    Ok(by_track.into_values().collect())
}

/// `GET /v1/library/tag-fixes?limit=`: what would change, file by file.
pub(crate) async fn list(State(s): State<Arc<NodeState>>, Query(q): Query<FixQuery>) -> ApiResult<FixList> {
    let limit = q.limit.unwrap_or(100).min(1000);
    let all = candidates(&s)?;
    let out = tokio::task::spawn_blocking(move || {
        let mut out = FixList::default();
        for (t, f) in all {
            match pm_library::tagwrite::preview(&t.file.path, &f) {
                Ok(changes) if changes.is_empty() => {}
                Ok(changes) => {
                    out.total += 1;
                    if out.files.len() < limit {
                        out.files.push(FileFix {
                            track_id: t.id.map(|i| i.0).unwrap_or_default(),
                            title: t.title.clone(),
                            path: t.file.path.display().to_string(),
                            changes: changes
                                .into_iter()
                                .map(|(key, now, then)| TagChange { key, now, then })
                                .collect(),
                        });
                    }
                }
                Err(_) => out.cannot += 1,
            }
        }
        out
    })
    .await
    .map_err(|e| ApiError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct FixApply {
    /// These tracks only; empty = every file in the list.
    #[serde(default)]
    pub track_ids: Vec<i64>,
}

/// `POST /v1/library/tag-fixes {track_ids?}`: write the fixes (each file's tags backed up first).
pub(crate) async fn apply(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<FixApply>,
) -> ApiResult<crate::identify::WriteReport> {
    let all = candidates(&s)?;
    let dir = crate::identify::backup_dir(&s);
    let report = tokio::task::spawn_blocking(move || {
        let mut report = crate::identify::WriteReport {
            backup_dir: dir.display().to_string(),
            ..Default::default()
        };
        for (t, f) in all {
            let id = t.id.map(|i| i.0).unwrap_or_default();
            if !req.track_ids.is_empty() && !req.track_ids.contains(&id) {
                continue;
            }
            if matches!(pm_library::tagwrite::preview(&t.file.path, &f), Ok(c) if c.is_empty()) {
                continue;
            }
            match pm_library::tagwrite::write_fixes(&t.file.path, &f, &dir) {
                Ok(_) => report.written += 1,
                Err(e) => report.skipped.push(crate::identify::WriteSkip {
                    track_id: id,
                    path: t.file.path.display().to_string(),
                    reason: e.to_string(),
                }),
            }
        }
        report
    })
    .await
    .map_err(|e| ApiError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}
