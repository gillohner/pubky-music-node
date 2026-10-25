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
}

impl From<reqwest::Error> for SyncError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest's message names the URL, never the bearer token
        Self::Unreachable(e.to_string())
    }
}

type Result<T> = std::result::Result<T, SyncError>;

/// The node, with this device's token.
pub struct Node {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Node {
    /// `base` like `http://192.168.1.20:7788`.
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        })
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
        let mut rb = self.req(m, path);
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

/// Send what was done offline, oldest first. Stops at the first error that may pass (the node
/// went away); what wasn't sent stays queued. Returns how many were sent or refused for good.
pub async fn flush(store: &Store, node: &Node) -> Result<usize> {
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
            Event::Favourite {
                track_id, on: true, ..
            } => node
                .json(
                    Method::PUT,
                    "/v1/favourites",
                    Some(json!({ "track_id": track_id })),
                )
                .await
                .map(drop),
            Event::Favourite {
                track_id, on: false, ..
            } => node
                .json(
                    Method::DELETE,
                    &format!("/v1/favourites?track_id={track_id}"),
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
    let mut rb = node.req(Method::GET, &format!("/v1/library/tracks/{id}/download{query}"));
    if let (true, Some(et)) = (have > 0, etag.as_deref()) {
        rb = rb
            .header(header::RANGE, format!("bytes={have}-"))
            .header(header::IF_RANGE, et.trim());
    }
    let mut resp = rb.send().await?;
    let append = match resp.status() {
        StatusCode::ACCEPTED => return Ok(Got::Preparing),
        StatusCode::PARTIAL_CONTENT => true,
        StatusCode::OK => false,
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
    while let Some(chunk) = resp.chunk().await? {
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    f.sync_all().await?;
    drop(f);
    tokio::fs::rename(&part, &dest).await?;
    let _ = tokio::fs::remove_file(&etag_file).await;
    let bytes = tokio::fs::metadata(&dest).await?.len();
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
    for (path, file) in [
        ("/", "index.html"),
        ("/app.js", "app.js"),
        ("/app.css", "app.css"),
        ("/icon.svg", "icon.svg"),
        ("/manifest.webmanifest", "manifest.webmanifest"),
    ] {
        let got = async {
            let r = Node::checked(node.req(Method::GET, path)).await?;
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

async fn put_manifest(store: &Store, node: &Node) -> Result<Value> {
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
    let me = node.get("/v1/me").await.ok();
    let device = store.index().device_id;
    node.json(
        Method::PUT,
        &format!("/v1/devices/{device}"),
        Some(json!({ "name": opts.name, "kind": opts.kind })),
    )
    .await?;
    let mut report = SyncReport {
        sent: flush(store, node).await?,
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

    let keep = store.index().keep;
    let mut playlists = std::collections::BTreeMap::new();
    for p in &keep.playlists {
        match node.get(&format!("/v1/playlists/{p}")).await {
            Ok(v) => {
                playlists.insert(p.clone(), v);
            }
            Err(e @ SyncError::Unreachable(_)) => return Err(e),
            // Deleted on the node: nothing to keep
            Err(_) => {}
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
    fn what_is_refused_for_good() {
        assert!(refused_for_good(&SyncError::Status(404, String::new())));
        assert!(refused_for_good(&SyncError::Status(400, String::new())));
        assert!(!refused_for_good(&SyncError::Status(401, String::new())));
        assert!(!refused_for_good(&SyncError::Status(503, String::new())));
        assert!(!refused_for_good(&SyncError::Unreachable(String::new())));
    }
}
