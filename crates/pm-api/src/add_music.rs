//! Library → Add music (STEWARD-BUNDLED-HOMESERVER.md §7): upload files from the browser
//! (resumable, a file at a time), or copy a folder on the machine, into the homeserver's private
//! storage (`/priv/pubky.music/files/<hash>-<name>`), from where they reach the library:
//! - **bundled homeserver:** its files folder is mounted read-only and the owner's music folder
//!   there is a library folder ([`crate::storage::ensure_homeserver_folder`]): no second copy;
//! - **another homeserver:** the file is also kept in the node's "From homeserver" folder.
//!
//! The third way, a folder used in place, is the library folders as they are.

use crate::state::NodeState;
use crate::types::UploadProgress;
use crate::{ApiError, ApiResult};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_pubky::homeserver::PRIVATE_FILES_DIR;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What an upload says when the homeserver is full (507).
pub const FULL: &str =
    "your homeserver is full: free some space there, or ask whoever runs it for a bigger quota";

/// Whether a store error is the homeserver being full.
pub(crate) fn is_full_error(e: &str) -> bool {
    e.ends_with(FULL)
}

/// What a chunk may be at most; the route takes a little more.
pub const CHUNK: u64 = 4 << 20;
pub const CHUNK_LIMIT: usize = 8 << 20;
/// Unfinished uploads are dropped after this.
const KEEP: Duration = Duration::from_secs(24 * 3600);

/// One file coming from the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upload {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub relative_path: Option<String>,
    pub size: u64,
    pub offset: u64,
    /// `receiving`, `storing`, `stored` or `failed`.
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    /// It failed because the homeserver is full.
    #[serde(default)]
    pub full: bool,
    /// The homeserver path it was stored at.
    #[serde(default)]
    pub stored_as: Option<String>,
    #[serde(skip)]
    touched: Option<Instant>,
    #[serde(skip)]
    writing: bool,
}

pub type Uploads = std::sync::Mutex<HashMap<String, Upload>>;

/// All uploads at once, for the job chip.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UploadTotals {
    pub files: usize,
    pub stored: usize,
    pub failed: usize,
    pub bytes_total: u64,
    pub bytes_done: u64,
}

pub fn totals(s: &NodeState) -> UploadTotals {
    let u = s.music_uploads.lock().expect("uploads");
    let mut t = UploadTotals::default();
    for up in u.values() {
        t.files += 1;
        t.bytes_total += up.size;
        t.bytes_done += up.offset;
        match up.state.as_str() {
            "stored" => t.stored += 1,
            "failed" => t.failed += 1,
            _ => {}
        }
    }
    t
}

fn parts_dir(s: &NodeState) -> PathBuf {
    s.data_dir().join("uploads")
}

fn part_path(s: &NodeState, id: &str) -> PathBuf {
    parts_dir(s).join(format!("{id}.part"))
}

/// A file name with anything but letters, digits, `.`, `-` and `_` as `_` (as files.rs names
/// homeserver files), and no path.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.trim_start_matches('.').chars().take(180).collect()
}

fn blake3_hex(bytes: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(bytes);
    h.finalize().to_hex().to_string()
}

fn needs_session(s: &NodeState) -> Result<Arc<crate::sync::PubkySync>, ApiError> {
    s.sync_handle().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "sign in to your homeserver first (Settings › Pubky)",
        )
    })
}

/// Homeserver files by `<hash prefix>` → size.
async fn present(sync: &crate::sync::PubkySync) -> Result<HashMap<String, Option<u64>>, String> {
    let list = sync.list_private_files().await.map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .filter_map(|f| {
            let name = f.path.rsplit('/').next()?.to_string();
            let (h, _) = crate::files::split_remote_name(&name)?;
            Some((h.to_string(), f.size))
        })
        .collect())
}

/// What happened to one file.
pub(crate) enum Stored {
    /// Sent to the homeserver, at this path.
    Sent(String),
    /// The homeserver already had the same content.
    Already(String),
}

/// Stores one file's bytes on the homeserver (unless the same content is there), and for an
/// external homeserver also in the node's "From homeserver" folder.
pub(crate) async fn store(
    s: &Arc<NodeState>,
    sync: &crate::sync::PubkySync,
    have: &HashMap<String, Option<u64>>,
    name: &str,
    relative: Option<&str>,
    bytes: Vec<u8>,
) -> Result<Stored, String> {
    if bytes.is_empty() {
        return Err(format!("{name}: the file is empty"));
    }
    let len = bytes.len() as u64;
    let safe = safe_name(name);
    let (bytes, hash) = tokio::task::spawn_blocking(move || {
        let h = blake3_hex(&bytes);
        (bytes, h)
    })
    .await
    .map_err(|e| e.to_string())?;
    let remote = format!("{PRIVATE_FILES_DIR}{}-{safe}", &hash[..16]);
    let bundled = crate::storage::homeserver_music_dir(s).is_some();
    if have
        .get(&hash[..16])
        .is_some_and(|sz| *sz == Some(len) || sz.is_none())
    {
        return Ok(Stored::Already(remote));
    }
    // To the node's own homeserver there is no uplink to spare; to another one, pace the copy.
    let rate = if bundled { 0 } else { crate::files::copy_rate(s) };
    let keep_local = (!bundled).then(|| bytes.clone());
    if let Err(e) = sync
        .upload_private_paced(&remote, bytes, (rate > 0).then_some(rate))
        .await
    {
        if e.is_full() {
            crate::storage::note_full(s, true);
            return Err(format!("{name}: {FULL}"));
        }
        return Err(format!("{name}: {e}"));
    }
    crate::storage::note_full(s, false);
    match sync.stat_private(&remote).await {
        Ok(Some(kept)) if kept != len => {
            return Err(format!(
                "{name}: the homeserver kept {kept} of {len} bytes; try again"
            ))
        }
        _ => {}
    }
    if let Some(bytes) = keep_local {
        // Into the "From homeserver" folder, under the dropped folder's own path when given
        let rel = relative
            .map(|r| {
                r.split(['/', '\\'])
                    .filter(|p| !p.is_empty() && *p != "." && *p != "..")
                    .map(safe_name)
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![safe.clone()]);
        let mut dest = s.mirror_dir.clone();
        for p in &rel {
            dest.push(p);
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
        tokio::fs::write(&dest, bytes).await.map_err(|e| e.to_string())?;
        let root = s.mirror_dir.clone();
        let known = s
            .library
            .lock()
            .expect("library poisoned")
            .list_folders()
            .map(|f| f.iter().any(|(p, _)| *p == root))
            .unwrap_or(false);
        if !known {
            let _ = s.library.lock().expect("library poisoned").add_folder(&root);
            s.watch_more(&root);
        }
        let _ = s.start_scan(vec![root]);
    }
    Ok(Stored::Sent(remote))
}

// ---- uploads from the browser ----

#[derive(Debug, Clone, Deserialize)]
pub struct NewUpload {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub relative_path: Option<String>,
    /// The file's BLAKE3 (hex), when the page computed it: the same content already on the
    /// homeserver is not sent again.
    #[serde(default)]
    pub blake3: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadStarted {
    pub id: Option<String>,
    pub offset: u64,
    pub chunk_size: u64,
    /// The homeserver has this content already; nothing to send.
    #[serde(default)]
    pub already: bool,
}

fn expire(u: &mut HashMap<String, Upload>, s: &NodeState) {
    let old: Vec<String> = u
        .values()
        .filter(|x| x.touched.is_none_or(|t| t.elapsed() > KEEP))
        .map(|x| x.id.clone())
        .collect();
    for id in old {
        u.remove(&id);
        let _ = std::fs::remove_file(part_path(s, &id));
    }
}

/// `POST /v1/library/uploads {name, size, relative_path?, blake3?}`.
pub(crate) async fn start(
    State(s): State<Arc<NodeState>>,
    Json(b): Json<NewUpload>,
) -> ApiResult<UploadStarted> {
    let sync = needs_session(&s)?;
    let name = safe_name(&b.name);
    if name.is_empty() || !pm_library::tags::is_supported(std::path::Path::new(&name)) {
        return Err(ApiError::bad_request(format!(
            "{}: not an audio file the library plays",
            b.name
        )));
    }
    if b.size == 0 {
        return Err(ApiError::bad_request("the file is empty"));
    }
    if let Some(h) = b.blake3.as_deref().map(str::to_ascii_lowercase) {
        if h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()) {
            let have = present(&sync)
                .await
                .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
            if have
                .get(&h[..16])
                .is_some_and(|sz| sz.is_none_or(|n| n == b.size))
            {
                return Ok(Json(UploadStarted {
                    id: None,
                    offset: b.size,
                    chunk_size: CHUNK,
                    already: true,
                }));
            }
        }
    }
    tokio::fs::create_dir_all(parts_dir(&s))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let id = crate::tidal::random_urlsafe(12);
    tokio::fs::write(part_path(&s, &id), b"")
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut u = s.music_uploads.lock().expect("uploads");
    expire(&mut u, &s);
    u.insert(
        id.clone(),
        Upload {
            id: id.clone(),
            name,
            relative_path: b.relative_path.map(|r| r.chars().take(400).collect()),
            size: b.size,
            offset: 0,
            state: "receiving".into(),
            error: None,
            full: false,
            stored_as: None,
            touched: Some(Instant::now()),
            writing: false,
        },
    );
    Ok(Json(UploadStarted {
        id: Some(id),
        offset: 0,
        chunk_size: CHUNK,
        already: false,
    }))
}

fn get_upload(s: &NodeState, id: &str) -> Result<Upload, ApiError> {
    s.music_uploads
        .lock()
        .expect("uploads")
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("no such upload (it may have expired: start it again)"))
}

/// `GET /v1/library/uploads/{id}`.
pub(crate) async fn get(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<Upload> {
    Ok(Json(get_upload(&s, &id)?))
}

/// `GET /v1/library/uploads`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<serde_json::Value> {
    let ups: Vec<Upload> = s
        .music_uploads
        .lock()
        .expect("uploads")
        .values()
        .cloned()
        .collect();
    Ok(Json(serde_json::json!({"totals": totals(&s), "uploads": ups})))
}

/// `DELETE /v1/library/uploads/{id}`.
pub(crate) async fn cancel(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let removed = s.music_uploads.lock().expect("uploads").remove(&id);
    if removed.is_none() {
        return Err(ApiError::not_found("no such upload"));
    }
    let _ = tokio::fs::remove_file(part_path(&s, &id)).await;
    Ok(Json(serde_json::json!({"cancelled": id})))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkAt {
    pub offset: u64,
}

/// `PUT /v1/library/uploads/{id}?offset=N`: the next chunk. A wrong `offset` answers 409 with
/// the one to send from; the last chunk stores the file.
pub(crate) async fn put_chunk(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Query(at): Query<ChunkAt>,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let (size, expected) = {
        let mut u = s.music_uploads.lock().expect("uploads");
        let up = u
            .get_mut(&id)
            .ok_or_else(|| ApiError::not_found("no such upload"))?;
        if up.state != "receiving" || up.writing || at.offset != up.offset {
            let v = serde_json::json!({"offset": up.offset, "state": up.state});
            return Ok((StatusCode::CONFLICT, Json(v)).into_response());
        }
        if body.len() as u64 > CHUNK || up.offset + body.len() as u64 > up.size {
            return Err(ApiError::bad_request(
                "the chunk is too big or goes past the file's size",
            ));
        }
        up.writing = true;
        (up.size, up.offset)
    };
    let write = async {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(part_path(&s, &id))
            .await?;
        f.write_all(&body).await?;
        f.flush().await
    };
    let res = write.await;
    let offset = {
        let mut u = s.music_uploads.lock().expect("uploads");
        let up = u
            .get_mut(&id)
            .ok_or_else(|| ApiError::not_found("no such upload"))?;
        up.writing = false;
        up.touched = Some(Instant::now());
        if let Err(e) = res {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("keeping the chunk: {e}"),
            ));
        }
        up.offset = expected + body.len() as u64;
        if up.offset == size {
            up.state = "storing".into();
        }
        up.offset
    };
    if offset == size {
        let s2 = s.clone();
        let id2 = id.clone();
        tokio::spawn(async move { finish(&s2, &id2).await });
    }
    Ok(Json(
        serde_json::json!({"offset": offset, "state": if offset == size { "storing" } else { "receiving" }}),
    )
    .into_response())
}

async fn finish(s: &Arc<NodeState>, id: &str) {
    let set = |f: &dyn Fn(&mut Upload)| {
        if let Some(u) = s.music_uploads.lock().expect("uploads").get_mut(id) {
            f(u);
        }
    };
    let Ok(up) = get_upload(s, id) else { return };
    let outcome = async {
        let sync = s.sync_handle().ok_or("signed out of the homeserver meanwhile")?;
        let bytes = tokio::fs::read(part_path(s, id))
            .await
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 != up.size {
            return Err(format!(
                "{}: received {} of {} bytes",
                up.name,
                bytes.len(),
                up.size
            ));
        }
        let have = present(&sync).await?;
        store(s, &sync, &have, &up.name, up.relative_path.as_deref(), bytes).await
    }
    .await;
    let _ = tokio::fs::remove_file(part_path(s, id)).await;
    match outcome {
        Ok(Stored::Sent(p)) | Ok(Stored::Already(p)) => {
            // In the library's folders before it says "stored"
            crate::storage::ensure_homeserver_folder(s, true).await;
            set(&|u| {
                u.state = "stored".into();
                u.stored_as = Some(p.clone());
            });
        }
        Err(e) => set(&|u| {
            u.state = "failed".into();
            u.full = is_full_error(&e);
            u.error = Some(e.clone());
        }),
    }
}

// ---- copy from a folder ----

#[derive(Debug, Clone, Deserialize)]
pub struct NewImport {
    pub path: String,
    #[serde(default)]
    pub dry_run: bool,
}

fn claim(s: &NodeState, job: &UploadProgress) -> Result<(), ApiError> {
    let mut jobs = s.upload_jobs.lock().expect("upload jobs");
    jobs.retain(|_, j| !j.finished);
    if let Some(running) = jobs.values().next() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "another copy is running ({} of {} files); wait for it or cancel it",
                running.done + running.skipped,
                running.total
            ),
        ));
    }
    jobs.insert(job.id.clone(), job.clone());
    Ok(())
}

/// `POST /v1/library/imports {path, dry_run?}`: copy a folder's audio files into the homeserver
/// (a job of kind `import`; progress and cancel as for the other copies, `GET
/// /v1/pubky/files/upload/{id}` and `POST /v1/pubky/files/jobs/{id}/cancel`). Files already
/// there are skipped as they are found.
pub(crate) async fn import(
    State(s): State<Arc<NodeState>>,
    Json(b): Json<NewImport>,
) -> ApiResult<UploadProgress> {
    let root = tokio::fs::canonicalize(b.path.trim())
        .await
        .map_err(|e| ApiError::bad_request(format!("{}: {e}", b.path.trim())))?;
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "{} is not a folder",
            root.display()
        )));
    }
    let r2 = root.clone();
    let mut files: Vec<(PathBuf, u64)> = tokio::task::spawn_blocking(move || {
        crate::storage::walk(&r2)
            .filter(|(p, _)| pm_library::tags::is_supported(p))
            .collect()
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    files.sort();
    if files.is_empty() {
        return Err(ApiError::bad_request(format!(
            "{} has no audio files the library plays",
            root.display()
        )));
    }
    let mut progress = UploadProgress {
        id: crate::tidal::random_urlsafe(9),
        kind: "import".into(),
        dest: Some(root.to_string_lossy().into_owned()),
        total: files.len(),
        bytes_total: files.iter().map(|(_, n)| n).sum(),
        ..Default::default()
    };
    if b.dry_run {
        return Ok(Json(progress));
    }
    let sync = needs_session(&s)?;
    claim(&s, &progress)?;
    let s2 = s.clone();
    let id = progress.id.clone();
    tokio::spawn(async move {
        let set = |f: &dyn Fn(&mut UploadProgress)| {
            if let Some(j) = s2.upload_jobs.lock().expect("upload jobs").get_mut(&id) {
                f(j);
            }
        };
        let cancelled = || {
            s2.upload_jobs
                .lock()
                .expect("upload jobs")
                .get(&id)
                .is_some_and(|j| j.cancelled)
        };
        let have = match present(&sync).await {
            Ok(h) => h,
            Err(e) => {
                set(&|j| {
                    j.errors
                        .push(format!("the homeserver did not list its files: {e}"));
                    j.finished = true;
                });
                return;
            }
        };
        for (path, size) in files {
            if cancelled() {
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel = path
                .strip_prefix(&root)
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            set(&|j| j.current = Some(name.clone()));
            let res = match tokio::fs::read(&path).await {
                Ok(bytes) => store(&s2, &sync, &have, &name, rel.as_deref(), bytes).await,
                Err(e) => Err(format!("{name}: {e}")),
            };
            match res {
                Ok(Stored::Sent(_)) => set(&|j| {
                    j.done += 1;
                    j.bytes_done += size;
                }),
                Ok(Stored::Already(_)) => set(&|j| {
                    j.skipped += 1;
                    j.bytes_done += size;
                }),
                // Full: every further file would be sent whole only to be refused
                Err(e) if is_full_error(&e) => {
                    set(&|j| {
                        j.failed += 1;
                        j.errors.push(format!("{e}; the files after it were not copied"));
                    });
                    break;
                }
                Err(e) => set(&|j| {
                    j.failed += 1;
                    j.errors.push(e.clone());
                }),
            }
        }
        crate::storage::ensure_homeserver_folder(&s2, true).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe() {
        assert_eq!(
            safe_name("Album/01 Bohemian Rhapsody.flac"),
            "01_Bohemian_Rhapsody.flac"
        );
        assert_eq!(safe_name("..\\..\\evil.flac"), "evil.flac");
        assert_eq!(safe_name(".hidden.mp3"), "hidden.mp3");
    }
}
