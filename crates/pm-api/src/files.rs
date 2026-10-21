//! The user's own audio on their homeserver's private storage: upload from
//! the node, list, and mirror everything that is missing locally into a
//! folder the scanner indexes. Private storage only (`/priv/pubky.music/files/`),
//! so uploads never become public (constraint 2) and the node only ever
//! serves its own user (constraint 3).

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use pm_model::TrackId;
use pm_pubky::homeserver::{RemoteFile, PRIVATE_FILES_DIR};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Remote name for a local file: content hash prefix plus the original name,
/// so the same file uploaded twice lands on one path.
pub fn remote_name(bytes: &[u8], local: &Path) -> String {
    let hash = blake3_hex(bytes);
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{}-{}", &hash[..16], safe)
}

fn blake3_hex(bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

fn needs_session(s: &NodeState) -> Result<Arc<crate::sync::PubkySync>, ApiError> {
    s.sync_handle().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "no Pubky session: set `pubky.session_secret_file` in the node config",
        )
    })
}

pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<RemoteFile>> {
    let sync = needs_session(&s)?;
    let files = sync
        .list_private_files()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(files))
}

/// Upload a node-local file (by path, or a library track by id).
pub(crate) async fn upload(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<UploadFile>,
) -> ApiResult<UploadedFile> {
    let sync = needs_session(&s)?;
    let local: PathBuf = match (body.track_id, body.path.as_deref()) {
        (Some(id), _) => {
            s.library
                .lock()
                .expect("library poisoned")
                .get_track(TrackId(id))?
                .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?
                .file
                .path
        }
        (None, Some(p)) if !p.trim().is_empty() => PathBuf::from(p),
        _ => return Err(ApiError::bad_request("give `track_id` or `path`")),
    };
    if !pm_library::tags::is_supported(&local) {
        return Err(ApiError::bad_request(format!(
            "{} is not a supported audio file",
            local.display()
        )));
    }
    let bytes = tokio::fs::read(&local)
        .await
        .map_err(|e| ApiError::bad_request(format!("read {}: {e}", local.display())))?;
    let size = bytes.len() as u64;
    let remote = format!("{PRIVATE_FILES_DIR}{}", remote_name(&bytes, &local));
    sync.upload_private(&remote, bytes)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(UploadedFile { path: remote, size }))
}

/// `POST /v1/pubky/files/upload`: copy an album, some tracks or the whole library to the
/// homeserver's private storage, as a background job (`GET …/upload/{id}` for progress). Files
/// already there (same content) are skipped, so running it again only sends what is new.
pub(crate) async fn upload_many(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<UploadRequest>,
) -> ApiResult<UploadProgress> {
    let files: Vec<(PathBuf, u64)> = {
        let lib = s.library.lock().expect("library poisoned");
        let mut tracks = Vec::new();
        if body.all {
            let mut after = None;
            loop {
                let page = lib.list_tracks(after, 500)?;
                after = page.last().and_then(|t| t.id);
                let n = page.len();
                tracks.extend(page);
                if n < 500 {
                    break;
                }
            }
        } else if let Some(a) = body.album_id {
            tracks = lib.album_tracks(a)?;
        } else {
            for id in &body.track_ids {
                tracks.extend(lib.get_track(TrackId(*id))?);
            }
        }
        let mut seen = std::collections::HashSet::new();
        tracks
            .into_iter()
            .filter(|t| seen.insert(t.file.path.clone()))
            .map(|t| (t.file.path.clone(), t.file.size_bytes))
            .collect()
    };
    if files.is_empty() {
        return Err(ApiError::bad_request("nothing to upload"));
    }
    let mut progress = UploadProgress {
        id: crate::tidal::random_urlsafe(9),
        total: files.len(),
        bytes_total: files.iter().map(|(_, b)| b).sum(),
        ..Default::default()
    };
    if body.dry_run {
        return Ok(Json(progress));
    }
    let sync = needs_session(&s)?;
    {
        let mut jobs = s.upload_jobs.lock().expect("upload jobs");
        jobs.retain(|_, j| !j.finished);
        if let Some(running) = jobs.values().next() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!(
                    "an upload is running ({} of {} files)",
                    running.done + running.skipped,
                    running.total
                ),
            ));
        }
        jobs.insert(progress.id.clone(), progress.clone());
    }
    let s2 = s.clone();
    let id = progress.id.clone();
    tokio::spawn(async move {
        let set = |f: &dyn Fn(&mut UploadProgress)| {
            if let Some(j) = s2.upload_jobs.lock().expect("upload jobs").get_mut(&id) {
                f(j);
            }
        };
        let present: std::collections::HashSet<String> = match sync.list_private_files().await {
            Ok(l) => l.into_iter().map(|f| f.path).collect(),
            Err(e) => {
                let msg = e.to_string();
                set(&|j| {
                    j.errors
                        .push(format!("the homeserver did not list its files: {msg}"));
                    j.finished = true;
                });
                return;
            }
        };
        for (path, size) in files {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            set(&|j| j.current = Some(name.clone()));
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    let msg = format!("{name}: {e}");
                    set(&|j| {
                        j.failed += 1;
                        j.errors.push(msg.clone());
                    });
                    continue;
                }
            };
            let remote = format!("{PRIVATE_FILES_DIR}{}", remote_name(&bytes, &path));
            if present.contains(&remote) {
                set(&|j| {
                    j.skipped += 1;
                    j.bytes_done += size;
                });
                continue;
            }
            match sync.upload_private(&remote, bytes).await {
                Ok(()) => set(&|j| {
                    j.done += 1;
                    j.bytes_done += size;
                }),
                Err(e) => {
                    let msg = format!("{name}: {e}");
                    set(&|j| {
                        j.failed += 1;
                        j.errors.push(msg.clone());
                    });
                }
            }
        }
        set(&|j| {
            j.current = None;
            j.finished = true;
            let n = j.errors.len();
            if n > 5 {
                j.errors.drain(..n - 5);
            }
        });
    });
    progress.current = None;
    Ok(Json(progress))
}

/// `GET /v1/pubky/files/upload/{id}`.
pub(crate) async fn upload_progress(
    State(s): State<Arc<NodeState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<UploadProgress> {
    s.upload_jobs
        .lock()
        .expect("upload jobs")
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found("no such upload"))
}

/// Download every remote file that is not in the mirror folder yet, then
/// scan the folder. Returns what was fetched.
pub(crate) async fn mirror(State(s): State<Arc<NodeState>>) -> ApiResult<MirrorReport> {
    let sync = needs_session(&s)?;
    let report = mirror_into(&sync, &s.mirror_dir)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    if report.fetched > 0 {
        let _ = s.start_scan(vec![s.mirror_dir.clone()]);
    }
    Ok(Json(report))
}

pub async fn mirror_into(
    sync: &crate::sync::PubkySync,
    dir: &Path,
) -> Result<MirrorReport, crate::sync::SyncError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| crate::sync::SyncError::Store(format!("create {}: {e}", dir.display())))?;
    let mut report = MirrorReport::default();
    for f in sync.list_private_files().await? {
        let local = dir.join(f.name());
        let present = tokio::fs::metadata(&local)
            .await
            .map(|m| f.size.is_none_or(|sz| m.len() == sz))
            .unwrap_or(false);
        if present {
            report.present += 1;
            continue;
        }
        match sync.download_private(&f.path).await {
            Ok(bytes) => {
                let tmp = local.with_extension("part");
                if let Err(e) = tokio::fs::write(&tmp, &bytes).await {
                    report.errors.push(format!("{}: {e}", f.path));
                    continue;
                }
                if let Err(e) = tokio::fs::rename(&tmp, &local).await {
                    report.errors.push(format!("{}: {e}", f.path));
                    continue;
                }
                report.fetched += 1;
                report.bytes += bytes.len() as u64;
            }
            Err(e) => report.errors.push(format!("{}: {e}", f.path)),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_are_content_addressed_and_safe() {
        let a = remote_name(b"abc", Path::new("/music/My Song (live).flac"));
        let b = remote_name(b"abc", Path::new("/elsewhere/My Song (live).flac"));
        assert_eq!(a, b);
        assert!(a.ends_with("-My_Song__live_.flac"), "{a}");
        assert_eq!(a.len(), 16 + 1 + "My_Song__live_.flac".len());
        assert_ne!(
            remote_name(b"abd", Path::new("x.flac")),
            remote_name(b"abc", Path::new("x.flac"))
        );
    }
}
