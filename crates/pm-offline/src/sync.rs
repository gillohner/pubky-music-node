//! Bringing a device up to date with its node (STEWARD-DOWNLOADS.md): send what was done offline,
//! say what the device keeps and has (`PUT /v1/devices/{id}/downloads`), fetch what the node
//! says it still wants, drop what nothing keeps any more, and refresh the kept playlists,
//! favourites, covers and the web UI.

use crate::store::{now, Event, Format, KeptTrack, Store};
use reqwest::{header, Method, StatusCode};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// What can stop a sync.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("the node can't be reached: {0}")]
    Unreachable(String),
    #[error("the node answered {0}: {1}")]
    Status(u16, String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("unexpected answer from the node: {0}")]
    Format(String),
    /// A song's download waited too long (a slow transcode, a stalled answer). Only that song
    /// fails; the partial file is kept for the next sync.
    #[error("the download timed out: {0}")]
    TimedOut(String),
    /// The store belongs to another node: nothing was sent or changed.
    #[error("this device's downloads are from another node ({kept}), not this one ({node})")]
    OtherNode { kept: String, node: String },
}

impl From<reqwest::Error> for SyncError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest's message names the URL, never the bearer token
        Self::Unreachable(e.to_string())
    }
}

type Result<T> = std::result::Result<T, SyncError>;

/// How long a small JSON request, a cover or a web UI file may take.
const JSON_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the node may stay silent in the middle of an answer (a dropped Wi-Fi without a
/// reset): the sync then stops, keeps what it has, and the next one goes on from there. For a
/// download, a stall this long fails only that song.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a download may wait for the node's first answer: the node makes a smaller copy
/// before it answers, which takes minutes for a long song on a small machine.
const DOWNLOAD_WAIT: Duration = Duration::from_secs(15 * 60);

/// The web UI's files, kept for offline use: the node's path and the file name in `ui/`.
pub(crate) const UI_FILES: [(&str, &str); 5] = [
    ("/", "index.html"),
    ("/app.js", "app.js"),
    ("/app.css", "app.css"),
    ("/icon.svg", "icon.svg"),
    ("/manifest.webmanifest", "manifest.webmanifest"),
];

/// The node, with this device's token.
pub struct Node {
    base: String,
    token: String,
    http: reqwest::Client,
    /// For song files: no read timeout from reqwest (it would also cut the wait for the
    /// answer); `download` bounds the wait and each read itself.
    files: reqwest::Client,
    download_wait: Duration,
    read_timeout: Duration,
}

impl Node {
    /// `base` like `http://192.168.1.20:7788`.
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(READ_TIMEOUT)
            .build()?;
        let files = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
            files,
            download_wait: DOWNLOAD_WAIT,
            read_timeout: READ_TIMEOUT,
        })
    }

    /// Change how long a download waits for the node's answer and for each part of the file
    /// (15 minutes and 60 seconds by default).
    pub fn with_download_timeouts(mut self, wait: Duration, read: Duration) -> Self {
        self.download_wait = wait;
        self.read_timeout = read;
        self
    }

    fn req(&self, m: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(m, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    async fn checked(rb: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let r = rb.send().await?;
        if r.status().is_success() {
            return Ok(r);
        }
        let code = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| text.chars().take(200).collect());
        Err(SyncError::Status(code, msg))
    }

    async fn json(&self, m: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut rb = self.req(m, path).timeout(JSON_TIMEOUT);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = Self::checked(rb).await?;
        if r.status() == StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        let b = r.bytes().await?;
        if b.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&b).map_err(|e| SyncError::Format(e.to_string()))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.json(Method::GET, path, None).await
    }
}

/// Who this install is, for the node's device list.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Shown in Settings, e.g. "Pixel 8".
    pub name: String,
    /// `android`, `ios`, `desktop` or `browser`.
    pub kind: String,
}

/// What one sync did.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct SyncReport {
    /// Plays and hearts sent from the queue.
    pub sent: usize,
    pub downloaded: Vec<i64>,
    /// Smaller copies the node is still making (fetched at a later sync).
    pub preparing: Vec<i64>,
    pub removed: Vec<i64>,
    /// Songs that failed, and why.
    pub failed: Vec<(i64, String)>,
}

/// Is this refusal for good (drop the event) rather than "try again later"?
fn refused_for_good(e: &SyncError) -> bool {
    matches!(e, SyncError::Status(c, _) if (400..500).contains(c) && ![401, 403, 408, 429].contains(c))
}

/// The node's id from its `/v1/info`. A store that has none yet becomes this node's; a store
/// of another node is refused ([`SyncError::OtherNode`]) before anything is sent.
fn ensure_node(store: &Store, info: &Value) -> Result<()> {
    let id = info
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return Err(SyncError::Format("the node doesn't say its id (/v1/info)".into()));
    }
    let kept = store.index().node_id;
    if kept.is_empty() {
        store.update(|ix| ix.node_id = id)?;
    } else if kept != id {
        return Err(SyncError::OtherNode { kept, node: id });
    }
    Ok(())
}

/// Send what was done offline, oldest first, after checking the store is this node's. Stops at
/// the first error that may pass (the node went away); what wasn't sent stays queued. Returns
/// how many were sent or refused for good.
pub async fn flush(store: &Store, node: &Node) -> Result<usize> {
    ensure_node(store, &node.get("/v1/info").await?)?;
    flush_queue(store, node).await
}

async fn flush_queue(store: &Store, node: &Node) -> Result<usize> {
    let events = store.queued();
    let mut done = 0;
    let mut stop = None;
    for e in &events {
        let r = match e {
            Event::Play { track_id, at } => node
                .json(
                    Method::POST,
                    "/v1/library/history",
                    Some(json!({"track_id": track_id, "zone": "offline", "played_at": at})),
                )
                .await
                .map(drop),
            // With the time of the change: the node skips a clear older than the heart it has
            Event::Favourite {
                track_id,
                on: true,
                at,
            } => node
                .json(
                    Method::PUT,
                    "/v1/favourites",
                    Some(json!({ "track_id": track_id, "at": at })),
                )
                .await
                .map(drop),
            Event::Favourite {
                track_id,
                on: false,
                at,
            } => node
                .json(
                    Method::DELETE,
                    &format!("/v1/favourites?track_id={track_id}&at={at}"),
                    None,
                )
                .await
                .map(drop),
        };
        match r {
            Ok(()) => done += 1,
            Err(e) if refused_for_good(&e) => {
                tracing::info!("offline: the node refused a queued {e:?}; dropped");
                done += 1;
            }
            Err(e) => {
                stop = Some(e);
                break;
            }
        }
    }
    store.drop_sent(done)?;
    // The hearts still waiting are what the offline index shows on top of the node's list
    let waiting = store.queued();
    store.update(|ix| {
        ix.favourite_changes = waiting
            .iter()
            .filter_map(|e| match e {
                Event::Favourite { track_id, on, .. } => Some((*track_id, *on)),
                _ => None,
            })
            .collect();
    })?;
    match stop {
        Some(e) => Err(e),
        None => Ok(done),
    }
}

/// A file extension from the node's container name.
fn ext_of(container: &str) -> String {
    match container {
        "aac" | "alac" | "m4a" | "mp4" => "m4a".into(),
        "vorbis" | "ogg" => "ogg".into(),
        c if !c.is_empty() && c.len() <= 5 && c.bytes().all(|b| b.is_ascii_alphanumeric()) => {
            c.to_ascii_lowercase()
        }
        _ => "bin".into(),
    }
}

/// `Content-Range: bytes <first>-<last>/<total>`: the first byte, and the total length when the
/// node knows it.
fn content_range(h: &header::HeaderMap) -> Option<(u64, Option<u64>)> {
    let v = h.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    if last < first {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse::<u64>().ok().filter(|t| *t > last)?),
    };
    Some((first, total.or(Some(last + 1))))
}

enum Got {
    Done(KeptTrack),
    /// 202: the node is still making the smaller copy.
    Preparing,
}

/// Download one song (resuming a partial file), with the node's answer for it.
async fn download(store: &Store, node: &Node, id: i64, fmt: Option<&Format>) -> Result<Got> {
    let mut track = node.get(&format!("/v1/library/tracks/{id}")).await?;
    // The device keeps the name, not where the owner keeps the file
    if let Some(Value::String(p)) = track.pointer_mut("/file/path") {
        *p = p.rsplit(['/', '\\']).next().unwrap_or_default().to_string();
    }
    let container = track
        .pointer("/file/container")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (ext, format, query) = match fmt {
        None => (ext_of(&container), "original".to_string(), String::new()),
        Some(f) => (
            f.ext().to_string(),
            f.format.clone(),
            format!(
                "?format={}{}",
                f.format,
                f.bitrate.map(|b| format!("&bitrate={b}")).unwrap_or_default()
            ),
        ),
    };
    let files = store.files_dir();
    let name = format!("{id}.{ext}");
    let dest = files.join(&name);
    let part = files.join(format!("{name}.part"));
    let etag_file = files.join(format!("{name}.etag"));
    let have = tokio::fs::metadata(&part).await.map_or(0, |m| m.len());
    let etag = tokio::fs::read_to_string(&etag_file).await.ok();
    let mut rb = node
        .files
        .get(format!("{}/v1/library/tracks/{id}/download{query}", node.base))
        .bearer_auth(&node.token);
    if let (true, Some(et)) = (have > 0, etag.as_deref()) {
        rb = rb
            .header(header::RANGE, format!("bytes={have}-"))
            .header(header::IF_RANGE, et.trim());
    }
    let mut resp = tokio::time::timeout(node.download_wait, rb.send())
        .await
        .map_err(|_| SyncError::TimedOut(format!("no answer for song {id}")))??;
    // Whether the answer continues the partial file, and the whole file's length if known
    let (append, total) = match resp.status() {
        StatusCode::ACCEPTED => return Ok(Got::Preparing),
        StatusCode::PARTIAL_CONTENT => match content_range(resp.headers()) {
            Some((first, total)) if first == have => (true, total),
            _ => {
                // Not the rest of what the device has: ask for the whole file next time
                let _ = tokio::fs::remove_file(&etag_file).await;
                return Err(SyncError::Format(format!(
                    "the node sent another part of song {id} than asked for"
                )));
            }
        },
        StatusCode::OK => (false, resp.content_length()),
        StatusCode::RANGE_NOT_SATISFIABLE => {
            // The partial file doesn't fit the node's any more: start again next time
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_file(&etag_file).await;
            return Err(SyncError::Status(
                416,
                "the partial download no longer fits".into(),
            ));
        }
        s => {
            let text = resp.text().await.unwrap_or_default();
            return Err(SyncError::Status(s.as_u16(), text.chars().take(200).collect()));
        }
    };
    if let Some(et) = resp.headers().get(header::ETAG).and_then(|v| v.to_str().ok()) {
        tokio::fs::write(&etag_file, et).await?;
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&part)
        .await?;
    loop {
        let next = tokio::time::timeout(node.read_timeout, resp.chunk())
            .await
            .map_err(|_| SyncError::TimedOut(format!("song {id} stalled")))?;
        let Some(chunk) = next? else { break };
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    f.sync_all().await?;
    drop(f);
    let bytes = tokio::fs::metadata(&part).await?.len();
    match total {
        Some(t) if bytes < t => {
            // Cut short: the partial file stays, and the next sync resumes it
            return Err(SyncError::Format(format!(
                "song {id} stopped at {bytes} of {t} bytes"
            )));
        }
        Some(t) if bytes > t => {
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_file(&etag_file).await;
            return Err(SyncError::Format(format!(
                "song {id} came to {bytes} bytes, not {t}"
            )));
        }
        _ => {}
    }
    tokio::fs::rename(&part, &dest).await?;
    let _ = tokio::fs::remove_file(&etag_file).await;
    Ok(Got::Done(KeptTrack {
        track,
        file: name,
        bytes,
        format,
    }))
}

/// An album's cover, once per album; none is fine.
async fn fetch_cover(store: &Store, node: &Node, album: i64) -> Result<()> {
    if store.index().covers.contains_key(&album) {
        return Ok(());
    }
    let r = node
        .req(Method::GET, &format!("/v1/library/albums/{album}/cover"))
        .timeout(JSON_TIMEOUT)
        .send()
        .await?;
    if !r.status().is_success() {
        return Ok(());
    }
    let ext = match r
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
    {
        t if t.contains("png") => "png",
        t if t.contains("webp") => "webp",
        _ => "jpg",
    };
    let bytes = r.bytes().await?;
    let name = format!("{album}.{ext}");
    tokio::fs::write(store.art_dir().join(&name), &bytes).await?;
    store.update(|ix| {
        ix.covers.insert(album, name);
    })?;
    Ok(())
}

/// The web UI the node serves, kept for offline use (failures keep the previous copy).
async fn cache_ui(store: &Store, node: &Node) {
    for (path, file) in UI_FILES {
        let got = async {
            let r = Node::checked(node.req(Method::GET, path).timeout(JSON_TIMEOUT)).await?;
            Ok::<_, SyncError>(r.bytes().await?)
        }
        .await;
        match got {
            Ok(b) => {
                let tmp = store.ui_dir().join(format!("{file}.tmp"));
                if tokio::fs::write(&tmp, &b).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, store.ui_dir().join(file)).await;
                }
            }
            Err(e) => tracing::info!("offline: kept the old {file}: {e}"),
        }
    }
}

/// Tell the node what the device keeps and has. Songs whose file is gone (the system or the
/// person cleared it) are dropped from the index first, so the node asks for them again.
async fn put_manifest(store: &Store, node: &Node) -> Result<Value> {
    let files = store.files_dir();
    let gone: Vec<i64> = store
        .index()
        .tracks
        .iter()
        .filter(|(_, t)| !files.join(&t.file).is_file())
        .map(|(id, _)| *id)
        .collect();
    if !gone.is_empty() {
        tracing::info!("offline: {} kept songs lost their file", gone.len());
        store.update(|ix| {
            for id in &gone {
                ix.tracks.remove(id);
            }
        })?;
    }
    let ix = store.index();
    let have: Vec<Value> = ix
        .tracks
        .iter()
        .map(|(id, t)| json!({"track_id": id, "bytes": t.bytes, "format": t.format}))
        .collect();
    node.json(
        Method::PUT,
        &format!("/v1/devices/{}/downloads", ix.device_id),
        Some(json!({ "keep": ix.keep, "have": have })),
    )
    .await
}

/// One sync with the node. Per-song failures are in the report; an error means the node went
/// away (what was done so far is kept, and the next sync goes on from there).
pub async fn sync(store: &Store, node: &Node, opts: &SyncOptions) -> Result<SyncReport> {
    let info = node.get("/v1/info").await?;
    // Never another node's plays, hearts or keeps
    ensure_node(store, &info)?;
    let me = node.get("/v1/me").await.ok();
    let device = store.index().device_id;
    node.json(
        Method::PUT,
        &format!("/v1/devices/{device}"),
        Some(json!({ "name": opts.name, "kind": opts.kind })),
    )
    .await?;
    let mut report = SyncReport {
        sent: flush_queue(store, node).await?,
        ..Default::default()
    };
    cache_ui(store, node).await;

    let manifest = put_manifest(store, node).await?;
    let ids = |k: &str| -> Vec<i64> {
        manifest
            .get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_i64().or_else(|| x.get("track_id").and_then(Value::as_i64)))
                    .collect()
            })
            .unwrap_or_default()
    };
    for id in ids("unkept") {
        if let Some(path) = store.track_file(id) {
            let _ = tokio::fs::remove_file(path).await;
        }
        store.update(|ix| ix.tracks.remove(&id))?;
        report.removed.push(id);
    }
    let format = store.index().keep.format;
    for id in ids("wanted") {
        match download(store, node, id, format.as_ref()).await {
            Ok(Got::Done(kept)) => {
                let album = kept.track.pointer("/album/id").and_then(Value::as_i64);
                let old = store.update(|ix| ix.tracks.insert(id, kept.clone()))?;
                if let Some(old) = old.filter(|o| o.file != kept.file) {
                    let _ = tokio::fs::remove_file(store.files_dir().join(old.file)).await;
                }
                if let Some(a) = album {
                    let _ = fetch_cover(store, node, a).await;
                }
                report.downloaded.push(id);
            }
            Ok(Got::Preparing) => report.preparing.push(id),
            Err(e @ SyncError::Unreachable(_)) => return Err(e),
            Err(e) => report.failed.push((id, e.to_string())),
        }
    }
    // Covers for songs kept before a cover could be fetched
    let albums: Vec<i64> = store
        .index()
        .tracks
        .values()
        .filter_map(|t| t.track.pointer("/album/id").and_then(Value::as_i64))
        .collect();
    for a in albums {
        let _ = fetch_cover(store, node, a).await;
    }

    let ix = store.index();
    let keep = ix.keep;
    let mut playlists = std::collections::BTreeMap::new();
    for p in &keep.playlists {
        match node.get(&format!("/v1/playlists/{p}")).await {
            Ok(v) => {
                playlists.insert(p.clone(), v);
            }
            Err(e @ SyncError::Unreachable(_)) => return Err(e),
            // Deleted on the node: nothing to keep
            Err(SyncError::Status(404, _)) => {}
            // Anything else may pass: keep what the device had
            Err(e) => {
                tracing::info!("offline: kept the old playlist {p}: {e}");
                if let Some(old) = ix.playlists.get(p) {
                    playlists.insert(p.clone(), old.clone());
                }
            }
        }
    }
    let favourites = if keep.favourites {
        Some(node.get("/v1/favourites").await?)
    } else {
        None
    };
    store.update(|ix| {
        ix.playlists = playlists;
        ix.favourites = favourites;
        ix.node = Some(info);
        ix.me = me;
    })?;
    // Tell the node what the device has now
    put_manifest(store, node).await?;
    store.update(|ix| ix.synced_at = Some(now()))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(ext_of("flac"), "flac");
        assert_eq!(ext_of("alac"), "m4a");
        assert_eq!(ext_of("vorbis"), "ogg");
        assert_eq!(ext_of("../x"), "bin");
        assert_eq!(ext_of(""), "bin");
    }

    #[test]
    fn content_ranges() {
        let h = |v: &str| {
            let mut m = header::HeaderMap::new();
            m.insert(header::CONTENT_RANGE, v.parse().unwrap());
            m
        };
        assert_eq!(content_range(&h("bytes 300-999/1000")), Some((300, Some(1000))));
        assert_eq!(content_range(&h("bytes 300-999/*")), Some((300, Some(1000))));
        assert_eq!(content_range(&h("bytes 300-999/500")), None);
        assert_eq!(content_range(&h("bytes 9-3/10")), None);
        assert_eq!(content_range(&h("bytes */1000")), None);
        assert_eq!(content_range(&header::HeaderMap::new()), None);
    }

    #[test]
    fn what_is_refused_for_good() {
        assert!(refused_for_good(&SyncError::Status(404, String::new())));
        assert!(refused_for_good(&SyncError::Status(400, String::new())));
        assert!(!refused_for_good(&SyncError::Status(401, String::new())));
        assert!(!refused_for_good(&SyncError::Status(503, String::new())));
        assert!(!refused_for_good(&SyncError::Unreachable(String::new())));
    }
}
