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
pub(crate) fn safe_name(name: &str) -> String {
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

/// Homeserver files by `<hash prefix>` → size.
async fn present(target: &dyn crate::copy_guard::Target) -> Result<HashMap<String, Option<u64>>, String> {
    Ok(have_of(target.list().await?))
}

/// Homeserver files by `<hash prefix>` → size.
fn have_of(list: Vec<pm_pubky::homeserver::RemoteFile>) -> HashMap<String, Option<u64>> {
    list.into_iter()
        .filter_map(|f| {
            let name = f.path.rsplit('/').next()?.to_string();
            let (h, _) = crate::files::split_remote_name(&name)?;
            Some((h.to_string(), f.size))
        })
        .collect()
}

/// Where a song with content `hash` goes at `dest` without replacing anything: `dest` itself when
/// free, `None` when the same song is there already, else `name (2).ext`, `name (3).ext`, ….
fn free_name(dest: &std::path::Path, hash: &str) -> Option<std::path::PathBuf> {
    let same = |p: &std::path::Path| std::fs::read(p).is_ok_and(|b| blake3_hex(&b) == hash);
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = dest
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1.. {
        let p = if n == 1 {
            dest.to_path_buf()
        } else {
            dest.with_file_name(format!("{stem} ({n}){ext}"))
        };
        if !p.exists() {
            return Some(p);
        }
        if same(&p) {
            return None;
        }
    }
    unreachable!("an endless range")
}

/// What happened to one file.
pub(crate) enum Stored {
    /// Sent to the homeserver, at this path.
    Sent(String),
    /// The homeserver already had the same content.
    Already(String),
}

/// Stores one file's bytes on the homeserver (unless the same content is there), and for an
/// external homeserver also in the node's "From homeserver" folder. What it sends is added to
/// `have`: the same content later in the job isn't sent again.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store(
    s: &Arc<NodeState>,
    target: &dyn crate::copy_guard::Target,
    room: &dyn crate::copy_guard::Room,
    have: &mut HashMap<String, Option<u64>>,
    name: &str,
    relative: Option<&str>,
    bytes: Vec<u8>,
    source: Option<&std::path::Path>,
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
    // Remembered for the file it came from: the next copy knows it is there without reading it
    if let Some(p) = source {
        crate::files::remember_hash(s, p, &hash);
    }
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
    // Listed at another size: what is there now decides (it may be under another name)
    if have.contains_key(&hash[..16]) {
        match crate::copy_guard::needs_sending(target, &remote, len).await {
            Ok(true) => {}
            Ok(false) => return Ok(Stored::Already(remote)),
            Err(e) => return Err(format!("{name}: couldn't check the copy there ({e})")),
        }
    }
    // Guarded: the reserve of the disk it lands on (the bundled homeserver's, else this node's
    // mirror), and nothing half written left there
    let rate = (rate > 0).then_some(rate);
    match crate::copy_guard::upload_guarded(s, target, room, &remote, bytes, rate).await {
        Ok(()) => {}
        Err(crate::copy_guard::Refused::Space(why)) => return Err(format!("{name}: {why}")),
        Err(crate::copy_guard::Refused::Full) => {
            crate::storage::note_full(s, true);
            return Err(format!("{name}: {FULL}"));
        }
        Err(crate::copy_guard::Refused::Short(kept)) => {
            return Err(format!(
                "{name}: the homeserver kept {kept} of {len} bytes; try again"
            ))
        }
        Err(crate::copy_guard::Refused::Other(e)) => return Err(format!("{name}: {e}")),
    }
    have.insert(hash[..16].to_string(), Some(len));
    crate::storage::note_full(s, false);
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
        // Never over another file: the same song there already is kept, anything else gets the
        // next free name
        let free = {
            let (dest, hash) = (dest.clone(), hash.clone());
            tokio::task::spawn_blocking(move || free_name(&dest, &hash))
                .await
                .map_err(|e| e.to_string())?
        };
        if let Some(dest) = free {
            // Written beside it and moved into place: a failed write leaves no half a song
            let part = dest.with_file_name(format!(
                ".{}.part",
                dest.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            ));
            if let Err(e) = tokio::fs::write(&part, bytes).await {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e.to_string());
            }
            tokio::fs::rename(&part, &dest).await.map_err(|e| e.to_string())?;
        }
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
    let target = crate::copy_guard::target_of(&s).map_err(|_| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "sign in to your homeserver first (Settings › Pubky)",
        )
    })?;
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
            let have = present(target.as_ref())
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
    // The disk it lands on keeps its reserve (the bundled homeserver's, or this node's mirror):
    // refused before any of it is sent
    if let Some(why) = crate::copy_guard::room_for_store(&s)
        .space()
        .and_then(|sp| sp.too_little(b.size))
    {
        return Err(ApiError::new(StatusCode::INSUFFICIENT_STORAGE, why));
    }
    // And the disk it waits on until whole, with what the other uploads still have to send
    let coming: u64 = s
        .music_uploads
        .lock()
        .expect("uploads")
        .values()
        .filter(|u| u.state == "receiving")
        .map(|u| u.size - u.offset)
        .sum();
    if let Some(why) = crate::copy_guard::room_for_staging(&s, &parts_dir(&s))
        .space()
        .and_then(|sp| sp.too_little(coming + b.size))
    {
        return Err(ApiError::new(StatusCode::INSUFFICIENT_STORAGE, why));
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
    // The disk it waits on keeps its reserve while it arrives (other things write there too)
    if let Some(why) = crate::copy_guard::room_for_staging(&s, &parts_dir(&s))
        .space()
        .and_then(|sp| sp.too_little(body.len() as u64))
    {
        if let Some(up) = s.music_uploads.lock().expect("uploads").get_mut(&id) {
            up.writing = false;
        }
        return Err(ApiError::new(StatusCode::INSUFFICIENT_STORAGE, why));
    }
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
        let mut have = present(sync.as_ref()).await?;
        let room = crate::copy_guard::room_for_store(s);
        store(
            s,
            sync.as_ref(),
            room.as_ref(),
            &mut have,
            &up.name,
            up.relative_path.as_deref(),
            bytes,
            None,
        )
        .await
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
    // The disk the files land on: the bundled homeserver's, or this node's mirror of them
    let room = crate::copy_guard::room_for_store(&s);
    let target = crate::copy_guard::target_of(&s);
    // What the homeserver has already is skipped: counted out of the space needed
    let raw = match &target {
        Ok(t) => t.list().await.ok(),
        Err(_) => None,
    };
    // `<name> <size>` of what is there: files imported before their hashes were remembered are
    // known by the name they were stored under (`<hash16>-<safe name>`) and their size
    let named: std::collections::HashSet<(String, u64)> = raw
        .iter()
        .flatten()
        .filter_map(|f| {
            let (_, safe) = crate::files::split_remote_name(f.name())?;
            Some((safe.to_string(), f.size?))
        })
        .collect();
    let listed = raw.map(have_of);
    let needed = files
        .iter()
        .filter(|(p, _)| !crate::copy_guard::is_apple_double(p))
        .filter(|(p, n)| {
            !listed
                .as_ref()
                .is_some_and(|have| match crate::files::known_hash(&s, p) {
                    Some(h) => have.get(&h[..16]).is_some_and(|sz| sz.is_none_or(|k| k == *n)),
                    None => {
                        let file = p.file_name().map(|f| f.to_string_lossy().into_owned());
                        file.is_some_and(|f| named.contains(&(safe_name(&f), *n)))
                    }
                })
        })
        .map(|(_, n)| n)
        .sum();
    let fits = crate::copy_guard::plan(room.as_ref(), &mut progress, needed);
    if b.dry_run {
        return Ok(Json(progress));
    }
    fits?;
    let target = target?;
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
        let have = match listed {
            Some(h) => Ok(h),
            None => present(target.as_ref()).await,
        };
        let mut have = match have {
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
        let stop = |why: String| {
            set(&|j| {
                j.errors.push(why.clone());
                j.stopped_reason = Some(why.clone());
            })
        };
        let mut streak = crate::copy_guard::Streak::default();
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
            if !crate::copy_guard::is_audio(&path).await {
                set(&|j| j.not_audio += 1);
                continue;
            }
            // Never into the disk's reserve: checked right before each real upload (store)
            let res = match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    store(
                        &s2,
                        target.as_ref(),
                        room.as_ref(),
                        &mut have,
                        &name,
                        rel.as_deref(),
                        bytes,
                        Some(&path),
                    )
                    .await
                }
                Err(e) => Err(format!("{name}: {e}")),
            };
            match res {
                Ok(Stored::Sent(_)) => {
                    streak.ok();
                    set(&|j| {
                        j.done += 1;
                        j.bytes_done += size;
                    })
                }
                Ok(Stored::Already(_)) => {
                    streak.ok();
                    set(&|j| {
                        j.skipped += 1;
                        j.bytes_done += size;
                    })
                }
                // Full: every further file would be sent whole only to be refused
                Err(e) if is_full_error(&e) => {
                    set(&|j| j.failed += 1);
                    stop(format!("{e}; the files after it were not copied"));
                    break;
                }
                // The disk's reserve reached: nothing more is written
                Err(e) if crate::copy_guard::is_space_stop(&e) => {
                    stop(e);
                    break;
                }
                Err(e) => {
                    set(&|j| {
                        j.failed += 1;
                        j.errors.push(e.clone());
                    });
                    if let Some(why) = streak.failed(&e) {
                        stop(why);
                        break;
                    }
                }
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
