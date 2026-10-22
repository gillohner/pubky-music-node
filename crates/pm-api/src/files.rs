//! The user's own audio on their homeserver's private storage: copy from the
//! node, list, remove broken ones, and bring what this node lacks into a library
//! folder (a background job; files the library already has are matched by content
//! hash and not downloaded again). Private storage only (`/priv/pubky.music/files/`),
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

/// How fast copies to the homeserver go (bytes per second, 0 = full speed).
pub const COPY_RATE_KEY: &str = "copy.rate";
/// The default leaves room on a home uplink (often 20–40 Mbit/s) for the node's pages and the
/// music it streams to a browser away from home: a full-speed copy starved both (owner, 2026-10-22).
pub const DEFAULT_COPY_RATE: u64 = 2_500_000;

pub fn copy_rate(s: &NodeState) -> u64 {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(COPY_RATE_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_COPY_RATE)
}

/// Remote name for a local file: content hash prefix plus the original name,
/// so the same file uploaded twice lands on one path.
pub fn remote_name(bytes: &[u8], local: &Path) -> String {
    remote_name_for(&blake3_hex(bytes), &safe_file_name(local))
}

/// A file's name with anything but letters, digits, `.`, `-` and `_` turned into `_`.
fn safe_file_name(local: &Path) -> String {
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
    safe
}

/// `<first 16 hex of the hash>-<name>`, adding the prefix once: a file the node brought from the
/// homeserver already carries it (that is how `x-x-01.flac` names appeared, owner 2026-10-22).
fn remote_name_for(hash: &str, safe_name: &str) -> String {
    let prefix = &hash[..16];
    match split_remote_name(safe_name) {
        Some((h, _)) if h == prefix => safe_name.to_string(),
        _ => format!("{prefix}-{safe_name}"),
    }
}

/// A remote file name's hash prefix and original name, when it has one.
pub fn split_remote_name(name: &str) -> Option<(&str, &str)> {
    let (h, rest) = name.split_at_checked(17)?;
    let h = h.strip_suffix('-')?;
    (h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()) && !rest.is_empty()).then_some((h, rest))
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
        kind: "copy".into(),
        total: files.len(),
        bytes_total: files.iter().map(|(_, b)| b).sum(),
        ..Default::default()
    };
    if body.dry_run {
        return Ok(Json(progress));
    }
    let sync = needs_session(&s)?;
    claim_job(&s, &progress)?;
    let s2 = s.clone();
    let id = progress.id.clone();
    tokio::spawn(async move {
        let set = |f: &dyn Fn(&mut UploadProgress)| {
            if let Some(j) = s2.upload_jobs.lock().expect("upload jobs").get_mut(&id) {
                f(j);
            }
        };
        // Path → size: a file there with another size (an empty upload) is sent again
        let present: std::collections::HashMap<String, Option<u64>> = match sync.list_private_files().await {
            Ok(l) => l.into_iter().map(|f| (f.path, f.size)).collect(),
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
            if is_cancelled(&s2, &id) {
                break;
            }
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
            if bytes.is_empty() {
                let msg = format!("{name}: the file is empty, not copied");
                set(&|j| {
                    j.failed += 1;
                    j.errors.push(msg.clone());
                });
                continue;
            }
            let len = bytes.len() as u64;
            // Hashing a big FLAC is CPU work: off the threads that answer requests
            let (bytes, remote, hash) = {
                let p = path.clone();
                match tokio::task::spawn_blocking(move || {
                    let hash = blake3_hex(&bytes);
                    let r = format!(
                        "{PRIVATE_FILES_DIR}{}",
                        remote_name_for(&hash, &safe_file_name(&p))
                    );
                    (bytes, r, hash)
                })
                .await
                {
                    Ok(v) => v,
                    Err(_) => continue,
                }
            };
            remember_hash(&s2, &path, &hash);
            if present.get(&remote).is_some_and(|sz| *sz == Some(len)) {
                set(&|j| {
                    j.skipped += 1;
                    j.bytes_done += size;
                });
                continue;
            }
            let rate = copy_rate(&s2);
            match sync
                .upload_private_paced(&remote, bytes, (rate > 0).then_some(rate))
                .await
            {
                // Check what the homeserver kept
                Ok(()) => match sync.stat_private(&remote).await {
                    Ok(Some(kept)) if kept == len => set(&|j| {
                        j.done += 1;
                        j.bytes_done += size;
                    }),
                    Ok(kept) => {
                        let msg = format!(
                            "{name}: the homeserver kept {} of {len} bytes; run the copy again",
                            kept.unwrap_or(0)
                        );
                        set(&|j| {
                            j.failed += 1;
                            j.errors.push(msg.clone());
                        });
                    }
                    // Could not check: count it as sent
                    Err(_) => set(&|j| {
                        j.done += 1;
                        j.bytes_done += size;
                    }),
                },
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

/// Only one files job at a time (copy or bring): they share the uplink and the homeserver.
fn claim_job(s: &NodeState, job: &UploadProgress) -> Result<(), ApiError> {
    let mut jobs = s.upload_jobs.lock().expect("upload jobs");
    jobs.retain(|_, j| !j.finished);
    if let Some(running) = jobs.values().next() {
        let what = if running.kind == "bring" {
            "Bringing files here"
        } else {
            "A copy to your homeserver"
        };
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "{what} is running ({} of {} files); wait for it or cancel it",
                running.done + running.skipped,
                running.total
            ),
        ));
    }
    jobs.insert(job.id.clone(), job.clone());
    Ok(())
}

fn is_cancelled(s: &NodeState, id: &str) -> bool {
    s.upload_jobs
        .lock()
        .expect("upload jobs")
        .get(id)
        .is_some_and(|j| j.cancelled)
}

/// `POST /v1/pubky/files/jobs/{id}/cancel`: stop after the file in hand (a download stops at
/// once and leaves nothing half-written).
pub(crate) async fn cancel_job(
    State(s): State<Arc<NodeState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<UploadProgress> {
    let job = {
        let mut jobs = s.upload_jobs.lock().expect("upload jobs");
        let j = jobs
            .get_mut(&id)
            .ok_or_else(|| ApiError::not_found("no such job"))?;
        j.cancelled = true;
        j.clone()
    };
    if job.kind == "bring" {
        let _ = s
            .library
            .lock()
            .expect("library poisoned")
            .delete_setting(BRING_RESUME_KEY);
    }
    Ok(Json(job))
}

fn mtime_of(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Keep a library file's content hash (the copy job just computed it).
fn remember_hash(s: &NodeState, path: &Path, hash: &str) {
    if let Ok(m) = std::fs::metadata(path) {
        let _ =
            s.library
                .lock()
                .expect("library poisoned")
                .store_file_hash(path, m.len(), mtime_of(&m), hash);
    }
}

/// BLAKE3 of a whole file, read in pieces.
fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut f = std::fs::File::open(path)?;
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// A bring job that was running when the node stopped: `{dest}`; the node starts it again.
pub const BRING_RESUME_KEY: &str = "files.bring";

/// What the homeserver has and what this node lacks.
struct BringPlan {
    dest: PathBuf,
    /// (remote path, size, local file name).
    fetch: Vec<(String, u64, String)>,
    present: usize,
    present_bytes: u64,
    broken: usize,
}

/// The folder files are brought into: `From homeserver` in the first library folder, else the
/// node's data folder.
fn default_dest(s: &NodeState) -> PathBuf {
    s.library
        .lock()
        .expect("library poisoned")
        .list_folders()
        .ok()
        .and_then(|f| f.into_iter().find(|(_, on)| *on).map(|(p, _)| p))
        .map(|p| p.join("From homeserver"))
        .unwrap_or_else(|| s.mirror_dir.clone())
}

async fn plan_bring(
    s: &NodeState,
    sync: &crate::sync::PubkySync,
    dest: PathBuf,
) -> Result<BringPlan, String> {
    let remote = sync
        .list_private_files()
        .await
        .map_err(|e| format!("the homeserver did not list its files: {e}"))?;
    let mut plan = BringPlan {
        dest,
        fetch: Vec::new(),
        present: 0,
        present_bytes: 0,
        broken: 0,
    };
    // Names on the homeserver, to spot a second copy under a doubled prefix
    let names: std::collections::HashSet<String> = remote.iter().map(|f| f.name().to_string()).collect();
    let mut wanted = Vec::new();
    for f in remote {
        let name = f.name().to_string();
        let size = f.size.unwrap_or(0);
        let parts = split_remote_name(&name).map(|(h, rest)| (h.to_string(), rest.to_string()));
        let doubled = parts.as_ref().is_some_and(|(h, rest)| {
            split_remote_name(rest).is_some_and(|(h2, _)| h2 == h) && names.contains(rest.as_str())
        });
        if size == 0 || doubled {
            plan.broken += 1;
            continue;
        }
        wanted.push((f.path, size, parts));
    }
    // Library files of the same sizes, hashed (kept, so only the first plan reads them)
    let sizes: std::collections::HashSet<u64> = wanted.iter().map(|(_, sz, _)| *sz).collect();
    let candidates = s
        .library
        .lock()
        .expect("library poisoned")
        .files_of_sizes(&sizes)
        .map_err(|e| e.to_string())?;
    let mut have: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (path, size, mtime) in candidates {
        let cached = s
            .library
            .lock()
            .expect("library poisoned")
            .cached_file_hash(&path, size, mtime)
            .ok()
            .flatten();
        let hash = match cached {
            Some(h) => h,
            None => {
                let p = path.clone();
                let Ok(Ok(h)) = tokio::task::spawn_blocking(move || hash_file(&p)).await else {
                    continue;
                };
                let _ = s
                    .library
                    .lock()
                    .expect("library poisoned")
                    .store_file_hash(&path, size, mtime, &h);
                h
            }
        };
        have.insert(hash[..16].to_string());
    }
    for (path, size, parts) in wanted {
        let full = path.rsplit('/').next().unwrap_or(&path).to_string();
        let (hash16, original) = parts.unwrap_or_else(|| (String::new(), full.clone()));
        if !hash16.is_empty() && have.contains(&hash16) {
            plan.present += 1;
            plan.present_bytes += size;
            continue;
        }
        // Already brought (maybe not scanned yet): the same name and size in the folder
        let mut local = original.clone();
        match tokio::fs::metadata(plan.dest.join(&local)).await {
            Ok(m) if m.len() == size => {
                plan.present += 1;
                plan.present_bytes += size;
                continue;
            }
            // Another file under that name: keep the homeserver's full name instead
            Ok(_) => local = full,
            Err(_) => {}
        }
        plan.fetch.push((path, size, local));
    }
    Ok(plan)
}

/// `POST /v1/pubky/files/mirror {dest?, dry_run?}`: bring the homeserver's files this node does
/// not have into a library folder, as a background job (progress at `GET …/jobs/{id}`). Files the
/// library already has (same content) are not downloaded; the folder is scanned at the end.
pub(crate) async fn mirror(
    State(s): State<Arc<NodeState>>,
    body: Option<Json<MirrorRequest>>,
) -> ApiResult<UploadProgress> {
    let req = body.map(|Json(b)| b).unwrap_or(MirrorRequest {
        dest: None,
        dry_run: false,
    });
    let sync = needs_session(&s)?;
    let dest = match req.dest.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => {
            let p = PathBuf::from(d);
            if !p.is_absolute() {
                return Err(ApiError::bad_request("give the folder as a full path"));
            }
            p
        }
        None => default_dest(&s),
    };
    let plan = plan_bring(&s, &sync, dest)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
    let progress = UploadProgress {
        id: crate::tidal::random_urlsafe(9),
        kind: "bring".into(),
        dest: Some(plan.dest.to_string_lossy().into_owned()),
        total: plan.fetch.len(),
        bytes_total: plan.fetch.iter().map(|(_, b, _)| b).sum(),
        skipped: plan.present,
        broken: plan.broken,
        ..Default::default()
    };
    if req.dry_run || plan.fetch.is_empty() {
        let mut p = progress;
        p.finished = !req.dry_run;
        return Ok(Json(p));
    }
    claim_job(&s, &progress)?;
    start_bring(s.clone(), sync, plan, progress.id.clone());
    Ok(Json(progress))
}

fn start_bring(s: Arc<NodeState>, sync: Arc<crate::sync::PubkySync>, plan: BringPlan, id: String) {
    let _ = s.library.lock().expect("library poisoned").set_setting(
        BRING_RESUME_KEY,
        &serde_json::json!({"dest": plan.dest}).to_string(),
    );
    tokio::spawn(async move {
        let set = |f: &dyn Fn(&mut UploadProgress)| {
            if let Some(j) = s.upload_jobs.lock().expect("upload jobs").get_mut(&id) {
                f(j);
            }
        };
        let dest = plan.dest.clone();
        if let Err(e) = tokio::fs::create_dir_all(&dest).await {
            let msg = format!("could not create {}: {e}", dest.display());
            set(&|j| {
                j.errors.push(msg.clone());
                j.finished = true;
            });
            return;
        }
        let mut fetched = 0usize;
        for (remote, size, local) in plan.fetch {
            if is_cancelled(&s, &id) {
                break;
            }
            set(&|j| j.current = Some(local.clone()));
            let target = dest.join(&local);
            let part = dest.join(format!(".{local}.part"));
            let rate = copy_rate(&s);
            let mut got = 0u64;
            let r = sync
                .download_private_to(&remote, &part, (rate > 0).then_some(rate), |n| {
                    got += n;
                    set(&|j| j.bytes_done += n);
                    !is_cancelled(&s, &id)
                })
                .await;
            match r {
                Ok(n) if n == size || size == 0 => {
                    if let Err(e) = tokio::fs::rename(&part, &target).await {
                        let msg = format!("{local}: {e}");
                        set(&|j| {
                            j.failed += 1;
                            j.errors.push(msg.clone());
                        });
                    } else {
                        fetched += 1;
                        set(&|j| j.done += 1);
                    }
                }
                other => {
                    let _ = tokio::fs::remove_file(&part).await;
                    let msg = match other {
                        Ok(n) => format!("{local}: got {n} of {size} bytes"),
                        Err(e) => format!("{local}: {e}"),
                    };
                    set(&|j| {
                        j.bytes_done = j.bytes_done.saturating_sub(got);
                        if !j.cancelled {
                            j.failed += 1;
                            j.errors.push(msg.clone());
                        }
                    });
                }
            }
        }
        // Into the library: a folder outside the library folders becomes one
        if fetched > 0 {
            let inside = s
                .library
                .lock()
                .expect("library poisoned")
                .list_folders()
                .unwrap_or_default()
                .into_iter()
                .any(|(f, on)| on && dest.starts_with(&f));
            if !inside {
                let _ = s.library.lock().expect("library poisoned").add_folder(&dest);
                s.watch_more(&dest);
            }
            // A scan already running would miss the new files: wait for it
            for _ in 0..600 {
                match s.start_scan(vec![dest.clone()]) {
                    Ok(false) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
                    _ => break,
                }
            }
        }
        if !is_cancelled(&s, &id) {
            let _ = s
                .library
                .lock()
                .expect("library poisoned")
                .delete_setting(BRING_RESUME_KEY);
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
}

/// After a restart: carry on with a bring job the node was running (what already arrived is
/// matched and not fetched again).
pub fn resume_jobs(s: &Arc<NodeState>) {
    let Some(v) = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(BRING_RESUME_KEY)
        .ok()
        .flatten()
    else {
        return;
    };
    let Some(dest) = serde_json::from_str::<serde_json::Value>(&v)
        .ok()
        .and_then(|v| v["dest"].as_str().map(PathBuf::from))
    else {
        return;
    };
    let s = s.clone();
    tokio::spawn(async move {
        // The Pubky session comes back in the background after start-up
        let mut sync = None;
        for _ in 0..120 {
            if let Some(h) = s.sync_handle() {
                sync = Some(h);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let Some(sync) = sync else { return };
        let plan = match plan_bring(&s, &sync, dest).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("bringing files here did not resume: {e}");
                return;
            }
        };
        if plan.fetch.is_empty() {
            let _ = s
                .library
                .lock()
                .expect("library poisoned")
                .delete_setting(BRING_RESUME_KEY);
            return;
        }
        let progress = UploadProgress {
            id: crate::tidal::random_urlsafe(9),
            kind: "bring".into(),
            dest: Some(plan.dest.to_string_lossy().into_owned()),
            total: plan.fetch.len(),
            bytes_total: plan.fetch.iter().map(|(_, b, _)| b).sum(),
            skipped: plan.present,
            broken: plan.broken,
            ..Default::default()
        };
        if claim_job(&s, &progress).is_ok() {
            tracing::info!(
                files = progress.total,
                "bringing files here: carrying on after a restart"
            );
            start_bring(s.clone(), sync, plan, progress.id);
        }
    });
}

/// `GET /v1/pubky/files/jobs`: the files job running now, if any (the player bar's chip).
pub(crate) async fn current_job(State(s): State<Arc<NodeState>>) -> ApiResult<Option<UploadProgress>> {
    let jobs = s.upload_jobs.lock().expect("upload jobs");
    Ok(Json(jobs.values().find(|j| !j.finished).cloned()))
}

/// `DELETE /v1/pubky/files {paths}`: remove private files (broken uploads, doubled copies).
pub(crate) async fn remove(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<RemoveFiles>,
) -> ApiResult<Vec<RemoteFile>> {
    let sync = needs_session(&s)?;
    for p in &body.paths {
        if !p.starts_with(PRIVATE_FILES_DIR) || p.contains("..") {
            return Err(ApiError::bad_request(format!("not one of your music files: {p}")));
        }
    }
    for p in &body.paths {
        sync.delete_private(p)
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("{p}: {e}")))?;
    }
    let files = sync
        .list_private_files()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(files))
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
