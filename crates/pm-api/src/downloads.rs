//! Downloads to phones and laptops (STEWARD-DOWNLOADS.md): `GET /v1/library/tracks/{id}/download`.
//!
//! The original file, or a transcode when the device asks for one (`?format=aac&bitrate=256`),
//! with resumable ranges (`Range`, `If-Range` on the `ETag`) and the node's usual token rules: a
//! listener may download what it may play (the library). Files the node doesn't hold on disk (a
//! WebDAV share, a drive) and every transcode are written once to the download cache
//! (`<data>/downloads/`, capped, oldest out first) and served from there, so a resumed download
//! never starts over. The homeserver's files are library tracks too: they come through the node,
//! one place and one policy.
//!
//! Transcodes use `ffmpeg` when the node has it (the `downloads.ffmpeg` setting, else `ffmpeg` on
//! the PATH); without it they answer 501 and the original stays available. At most one transcode
//! per CPU runs at a time, each within a time limit (killed past it), and a file is never read
//! into memory whole: the cache fills and answers in bounded chunks. The browser stream
//! (`/v1/library/tracks/{id}/file`) and Subsonic's `stream` answer the same way ([`stream_file`]).

use crate::state::NodeState;
use crate::ApiError;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use pm_model::TrackId;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// The setting naming the ffmpeg program (default: `ffmpeg` on the PATH).
pub const FFMPEG_SETTING: &str = "downloads.ffmpeg";
/// The setting capping the download cache, in MiB (default 2048).
pub const CACHE_MB_SETTING: &str = "downloads.cache_mb";
const DEFAULT_CACHE_MB: u64 = 2048;
/// The setting for the longest one transcode may run, in seconds (default: [`transcode_limit`]).
pub const TRANSCODE_TIMEOUT_SETTING: &str = "downloads.transcode_timeout_s";
/// How long a transcode waits for a free slot before the answer is 503.
const SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// What one read or write of a file moves at most.
const CHUNK: usize = 256 * 1024;

/// `?format=&bitrate=`
#[derive(Debug, Deserialize, Default)]
pub struct DownloadQuery {
    /// `aac`, `mp3` or `opus`; absent or `original`: the file as it is.
    #[serde(default)]
    pub format: Option<String>,
    /// kbit/s, 64–320 (default 256; Opus 160).
    #[serde(default)]
    pub bitrate: Option<u32>,
}

/// A transcode target: ffmpeg's codec and muxer, the file's extension and MIME type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub format: &'static str,
    codec: &'static str,
    muxer: &'static str,
    pub ext: &'static str,
    pub mime: &'static str,
    pub kbps: u32,
}

/// The transcode a query asks for, `None` for the original.
pub fn target(q: &DownloadQuery) -> Result<Option<Target>, ApiError> {
    let f = q.format.as_deref().map(str::trim).unwrap_or("");
    let (format, codec, muxer, ext, mime, default) = match f {
        "" | "original" => return Ok(None),
        "aac" => ("aac", "aac", "ipod", "m4a", "audio/mp4", 256),
        "mp3" => ("mp3", "libmp3lame", "mp3", "mp3", "audio/mpeg", 256),
        "opus" => ("opus", "libopus", "ogg", "opus", "audio/ogg", 160),
        other => {
            return Err(ApiError::bad_request(format!(
                "format {other}: use original, aac, mp3 or opus"
            )))
        }
    };
    let kbps = q.bitrate.unwrap_or(default);
    if !(64..=320).contains(&kbps) {
        return Err(ApiError::bad_request("bitrate is 64 to 320 kbit/s"));
    }
    Ok(Some(Target {
        format,
        codec,
        muxer,
        ext,
        mime,
        kbps,
    }))
}

fn cache_dir(s: &NodeState) -> PathBuf {
    s.data_dir().join("downloads")
}

type Locks = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

fn locks() -> &'static Locks {
    static LOCKS: OnceLock<Locks> = OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

/// The lock of one cache file, held. Its entry goes when the last holder is done, so the map holds
/// only the files being made.
struct KeyLock {
    key: String,
    _held: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for KeyLock {
    fn drop(&mut self) {
        let mut map = locks().lock().expect("download locks");
        // The map's and ours: nobody else holds it or waits for it (both clone under this lock)
        if map.get(&self.key).is_some_and(|m| Arc::strong_count(m) == 2) {
            map.remove(&self.key);
        }
    }
}

/// One lock per cache file, so two devices asking for the same transcode make it once.
async fn lock_for(key: &str) -> KeyLock {
    let m = locks()
        .lock()
        .expect("download locks")
        .entry(key.to_string())
        .or_default()
        .clone();
    KeyLock {
        key: key.to_string(),
        _held: m.lock_owned().await,
    }
}

/// One slot per CPU: the transcodes running at once, across every caller.
fn transcode_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(max_transcodes()))
}

/// The transcodes that run at once: one per CPU.
pub fn max_transcodes() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}

/// The longest a transcode of a track `duration_ms` long may run: a minute, plus a quarter of its
/// length (four times real time, slow hardware included), half an hour when the length is unknown,
/// at most three hours.
pub fn transcode_limit(duration_ms: Option<u64>) -> std::time::Duration {
    let ms = duration_ms.map_or(30 * 60_000, |d| 60_000u64.saturating_add(d / 4));
    std::time::Duration::from_millis(ms).min(std::time::Duration::from_secs(3 * 3600))
}

fn ffmpeg(s: &NodeState) -> String {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(FFMPEG_SETTING)
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "ffmpeg".into())
}

fn transcode_timeout(s: &NodeState, duration_ms: Option<u64>) -> std::time::Duration {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(TRANSCODE_TIMEOUT_SETTING)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map_or_else(|| transcode_limit(duration_ms), std::time::Duration::from_secs)
}

fn cache_cap_bytes(s: &NodeState) -> u64 {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(CACHE_MB_SETTING)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_MB)
        * 1024
        * 1024
}

/// Oldest-first eviction until the cache is under its cap (the file just made stays).
fn evict(dir: &FsPath, cap: u64, keep: &FsPath) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            (m.is_file() && !e.file_name().to_string_lossy().ends_with(".part"))
                .then(|| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, p) in files {
        if total <= cap {
            break;
        }
        if p != keep && std::fs::remove_file(&p).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// The track's file on this machine: the library path itself when it is a local file, else a copy
/// in the cache (fetched once from its share or drive, in chunks).
pub(crate) async fn local_original(
    s: &NodeState,
    id: i64,
    path: &FsPath,
    size: u64,
    mtime: i64,
) -> Result<PathBuf, ApiError> {
    if path.is_absolute() && tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file()) {
        return Ok(path.to_path_buf());
    }
    let dir = cache_dir(s);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let key = format!("{id}-{mtime}-{size}-original.{ext}");
    let dest = dir.join(&key);
    let _held = lock_for(&key).await;
    if tokio::fs::metadata(&dest).await.is_ok() {
        return Ok(dest);
    }
    let resp = crate::webdav::fetch_library_file(s, path)
        .await
        .map_err(|e| ApiError::not_found(format!("file for track {id}: {e}")))?;
    let cap = cache_cap_bytes(s);
    fill_cache(&dir, &dest, resp, cap).await?;
    evict(&dir, cap, &dest);
    Ok(dest)
}

/// Writes the body into the cache chunk by chunk (never more than the cache's cap), then moves it
/// in place; nothing is left behind when it fails.
async fn fill_cache(
    dir: &FsPath,
    dest: &FsPath,
    mut resp: reqwest::Response,
    cap: u64,
) -> Result<(), ApiError> {
    use tokio::io::AsyncWriteExt;
    let io =
        |e: std::io::Error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("download cache: {e}"));
    tokio::fs::create_dir_all(dir).await.map_err(io)?;
    let part = dest.with_extension("part");
    let filled = async {
        let mut f = tokio::fs::File::create(&part).await.map_err(io)?;
        let mut written = 0u64;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("the file's source: {e}")))?
        {
            written += chunk.len() as u64;
            if written > cap {
                return Err(ApiError::new(
                    StatusCode::INSUFFICIENT_STORAGE,
                    "the file is bigger than the download cache",
                ));
            }
            f.write_all(&chunk).await.map_err(io)?;
        }
        f.flush().await.map_err(io)
    }
    .await;
    if let Err(e) = filled {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    tokio::fs::rename(&part, dest).await.map_err(io)
}

/// Transcode `src` (a track `duration_ms` long) to `t` into the cache (once), and return the
/// cached file. It waits for a free slot (one per CPU) and is killed past its time limit.
async fn transcoded(
    s: &NodeState,
    id: i64,
    src: &FsPath,
    (size, mtime, duration_ms): (u64, i64, Option<u64>),
    t: Target,
) -> Result<PathBuf, ApiError> {
    let dir = cache_dir(s);
    let key = format!("{id}-{mtime}-{size}-{}-{}.{}", t.format, t.kbps, t.ext);
    let dest = dir.join(&key);
    let _held = lock_for(&key).await;
    if tokio::fs::metadata(&dest).await.is_ok() {
        return Ok(dest);
    }
    let _slot = tokio::time::timeout(SLOT_WAIT, transcode_slots().acquire())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "every transcode slot is busy: try again in a minute, or download the original",
            )
        })?
        .expect("the slots are never closed");
    let io =
        |e: std::io::Error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("download cache: {e}"));
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    let part = dir.join(format!("{key}.part"));
    let program = ffmpeg(s);
    let limit = transcode_timeout(s, duration_ms);
    // kill_on_drop: past the limit the future is dropped, and ffmpeg with it
    let run = tokio::process::Command::new(&program)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(["-vn", "-map_metadata", "0", "-c:a", t.codec, "-b:a"])
        .arg(format!("{}k", t.kbps))
        .args(["-f", t.muxer])
        .arg(&part)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(out) = tokio::time::timeout(limit, run).await else {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "ffmpeg took longer than {} s for {} {}k and was stopped; the original is always available without ?format",
                limit.as_secs(),
                t.format,
                t.kbps
            ),
        ));
    };
    match out {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                format!("this node can't transcode: install ffmpeg (or set {FFMPEG_SETTING}); the original is always available without ?format"),
            ))
        }
        Err(e) => return Err(io(e)),
        Ok(o) if !o.status.success() => {
            let _ = tokio::fs::remove_file(&part).await;
            let why = String::from_utf8_lossy(&o.stderr);
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("ffmpeg couldn't make {} {}k: {}", t.format, t.kbps, why.trim().lines().last().unwrap_or("failed")),
            ));
        }
        Ok(_) => {}
    }
    tokio::fs::rename(&part, &dest).await.map_err(io)?;
    evict(&dir, cache_cap_bytes(s), &dest);
    Ok(dest)
}

/// A single byte range from a `Range: bytes=a-b` header (or a suffix `bytes=-n`), within `total`.
fn parse_range(v: &str, total: u64) -> Option<(u64, u64)> {
    let r = v.strip_prefix("bytes=")?.trim();
    if r.contains(',') || total == 0 {
        return None;
    }
    let (a, b) = r.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (total.saturating_sub(n.min(total)), total - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            total - 1
        } else {
            b.parse::<u64>().ok()?.min(total - 1)
        };
        (start, end)
    };
    (start <= end && start < total).then_some((start, end))
}

/// Serve `path` with ranges and an ETag, read in bounded chunks from where the range starts;
/// `disposition` is Content-Disposition's type and file name (`attachment`, `inline`).
pub(crate) async fn serve(
    path: &FsPath,
    headers: &HeaderMap,
    mime: &str,
    disposition: Option<(&str, &str)>,
) -> Result<Response, ApiError> {
    let io = |e: std::io::Error| ApiError::not_found(format!("download: {e}"));
    let meta = tokio::fs::metadata(path).await.map_err(io)?;
    let total = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let etag = format!("\"{total:x}-{mtime:x}\"");
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }
    // A range only applies to the same file (If-Range with our ETag, or no If-Range)
    let same = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v == etag);
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|_| same)
        .map(|v| parse_range(v, total));
    let (status, start, len) = match range {
        Some(Some((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b - a + 1),
        Some(None) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response())
        }
        None => (StatusCode::OK, 0, total),
    };
    let mut f = tokio::fs::File::open(path).await.map_err(io)?;
    f.seek(std::io::SeekFrom::Start(start)).await.map_err(io)?;
    let stream =
        futures_util::stream::unfold((f.take(len), vec![0u8; CHUNK]), |(mut r, mut buf)| async move {
            match r.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n])),
                    (r, buf),
                )),
                Err(e) => Some((Err(e), (r, buf))),
            }
        });
    let mut resp = Response::new(axum::body::Body::from_stream(stream));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    let hv = |v: &str| HeaderValue::from_str(v).unwrap_or_else(|_| HeaderValue::from_static(""));
    h.insert(header::CONTENT_TYPE, hv(mime));
    h.insert(header::CONTENT_LENGTH, hv(&len.to_string()));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::ETAG, hv(&etag));
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            hv(&format!("bytes {start}-{}/{total}", start + len - 1)),
        );
    }
    if let Some((kind, name)) = disposition {
        let safe: String = name
            .chars()
            .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
            .collect();
        h.insert(
            header::CONTENT_DISPOSITION,
            hv(&format!("{kind}; filename=\"{safe}\"")),
        );
    }
    Ok(resp)
}

/// A library track's file for a player (the browser stream, Subsonic's `stream`): the local file,
/// or the cached copy of one on a share or a drive, with ranges, never read whole.
pub(crate) async fn stream_file(
    s: &NodeState,
    id: i64,
    file: &pm_model::AudioFile,
    headers: &HeaderMap,
    mime: &str,
    disposition: Option<(&str, &str)>,
) -> Result<Response, ApiError> {
    let src = local_original(s, id, &file.path, file.size_bytes, file.mtime_unix).await?;
    serve(&src, headers, mime, disposition).await
}

/// `GET /v1/library/tracks/{id}/download[?format=aac|mp3|opus&bitrate=…]`.
pub(crate) async fn download(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
    Query(q): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let t = target(&q)?;
    let track = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    let file = &track.file;
    let src = local_original(&s, id, &file.path, file.size_bytes, file.mtime_unix).await?;
    let stem = file
        .path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("track-{id}"));
    match t {
        None => {
            let (mime, ext) = crate::subsonic::mime_for(&file.container);
            let name = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{stem}.{ext}"));
            serve(&src, &headers, mime, Some(("attachment", &name))).await
        }
        Some(t) => {
            let out = transcoded(
                &s,
                id,
                &src,
                (file.size_bytes, file.mtime_unix, file.duration_ms),
                t,
            )
            .await?;
            serve(
                &out,
                &headers,
                t.mime,
                Some(("attachment", &format!("{stem}.{}", t.ext))),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_bitrates() {
        let q = |f: Option<&str>, b: Option<u32>| DownloadQuery {
            format: f.map(str::to_string),
            bitrate: b,
        };
        assert_eq!(target(&q(None, None)).unwrap(), None);
        assert_eq!(target(&q(Some("original"), Some(1))).unwrap(), None);
        let aac = target(&q(Some("aac"), None)).unwrap().unwrap();
        assert_eq!((aac.ext, aac.mime, aac.kbps), ("m4a", "audio/mp4", 256));
        assert_eq!(target(&q(Some("opus"), None)).unwrap().unwrap().kbps, 160);
        assert_eq!(target(&q(Some("mp3"), Some(128))).unwrap().unwrap().kbps, 128);
        assert!(target(&q(Some("wav"), None)).is_err());
        assert!(target(&q(Some("mp3"), Some(32))).is_err());
        assert!(target(&q(Some("mp3"), Some(500))).is_err());
    }

    #[tokio::test]
    async fn a_lock_entry_goes_when_its_last_holder_is_done() {
        let key = "lock-entry-test";
        let held = || locks().lock().unwrap().contains_key(key);
        let first = lock_for(key).await;
        let waiter = tokio::spawn(async move {
            let _second = lock_for(key).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // The waiter still needs it
        drop(first);
        waiter.await.unwrap();
        assert!(!held(), "the map keeps only the files being made");
        let again = lock_for(key).await;
        assert!(held());
        drop(again);
        assert!(!held());
    }

    #[test]
    fn transcode_limits() {
        let s = std::time::Duration::from_secs;
        assert_eq!(transcode_limit(Some(0)), s(60));
        assert_eq!(transcode_limit(Some(240_000)), s(120));
        assert_eq!(transcode_limit(None), s(1800));
        assert_eq!(transcode_limit(Some(u64::MAX)), s(3 * 3600));
        assert!(max_transcodes() >= 1);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
    }
}
