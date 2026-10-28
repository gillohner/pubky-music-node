//! WebDAV shares as library roots (STEWARD-COMPAT-HA-MA.md §6.4): a Nextcloud, ownCloud or NAS
//! folder scanned into the library like a local folder and played where it is.
//!
//! - **Keys:** a song on a share is `webdav://<share id>/<path inside the share>` in the
//!   library's files table, so missing files and removal work by prefix as for folders.
//! - **Scanning:** the share is listed with `PROPFIND` (one level at a time), and each new or
//!   changed audio file is read with HTTP range requests, so only its tags are fetched
//!   ([`RangeReader`], `pm_library::Scanner::scan_remote`). Shares are rescanned every 6 hours.
//! - **Playing:** zones stream the file; browsers get it relayed. The password stays in the
//!   node (its settings), never in an answer or a log.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const SETTING: &str = "webdav.shares";
pub const SCHEME: &str = "webdav://";
const RESCAN_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Listing stops here (a share that big wants a local mount).
const MAX_ENTRIES: usize = 200_000;
/// What a range request fetches at least.
const CHUNK: u64 = 256 * 1024;

/// A share. `Debug` never shows the password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub id: String,
    pub name: String,
    /// The folder's URL (`https://cloud.example.org/remote.php/dav/files/me/Music/`).
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

impl std::fmt::Debug for Share {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Share")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &"<set>")
            .finish()
    }
}

impl Share {
    fn base(&self) -> Result<url::Url, String> {
        let mut u = url::Url::parse(&self.url).map_err(|e| e.to_string())?;
        if !u.path().ends_with('/') {
            let p = format!("{}/", u.path());
            u.set_path(&p);
        }
        Ok(u)
    }

    /// The library root of this share.
    pub fn root(&self) -> PathBuf {
        PathBuf::from(format!("{SCHEME}{}", self.id))
    }

    /// The URL of `rel` (a path inside the share, `/`-separated, not encoded).
    pub fn file_url(&self, rel: &str) -> Result<url::Url, String> {
        let mut u = self.base()?;
        {
            let mut segs = u
                .path_segments_mut()
                .map_err(|_| "not a folder URL".to_string())?;
            segs.pop_if_empty();
            for s in rel.split('/').filter(|s| !s.is_empty()) {
                if s == ".." || s == "." {
                    return Err("bad path".into());
                }
                segs.push(s);
            }
        }
        Ok(u)
    }

    fn auth(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        if self.user.is_empty() {
            rb
        } else {
            rb.basic_auth(&self.user, Some(&self.password))
        }
    }
}

pub type Shared = Arc<RwLock<Vec<Share>>>;

pub fn load(lib: &pm_library::Library) -> Vec<Share> {
    lib.get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

fn store(s: &NodeState, shares: &[Share]) -> Result<(), ApiError> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(shares).expect("serializes"))?;
    *s.webdav.write().expect("webdav") = shares.to_vec();
    Ok(())
}

/// A library key's share and path inside it.
pub fn split_key(key: &Path) -> Option<(String, String)> {
    let k = key.to_str()?.strip_prefix(SCHEME)?;
    let (id, rel) = k.split_once('/')?;
    Some((id.to_string(), rel.to_string()))
}

pub fn is_key(key: &Path) -> bool {
    key.to_str().is_some_and(|k| k.starts_with(SCHEME))
}

fn find(shared: &Shared, id: &str) -> Option<Share> {
    shared.read().ok()?.iter().find(|s| s.id == id).cloned()
}

/// `%XX` in a URL path, decoded.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- listing ----

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DavEntry {
    /// The href's path, decoded.
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
}

fn unref(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        _ => String::new(),
    }
}

/// A `PROPFIND` multistatus answer as entries (namespaces ignored: `d:href`, `D:href`, `href`).
pub(crate) fn parse_multistatus(xml: &str) -> Result<Vec<DavEntry>, String> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<DavEntry> = None;
    let mut text = String::new();
    let local = |n: &str| -> String { n.rsplit(':').next().unwrap_or("").to_ascii_lowercase() };
    loop {
        match r.read_event().map_err(|e| format!("not a WebDAV answer: {e}"))? {
            Event::Start(e) | Event::Empty(e) => {
                let name = local(e.name().as_ref());
                match name.as_str() {
                    "response" => {
                        cur = Some(DavEntry {
                            path: String::new(),
                            dir: false,
                            size: 0,
                            mtime: 0,
                        })
                    }
                    "collection" => {
                        if let Some(c) = cur.as_mut() {
                            c.dir = true;
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            Event::Text(t) => text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0)),
            Event::GeneralRef(g) => text.push_str(&unref(g.as_ref())),
            Event::End(e) => {
                let name = local(e.name().as_ref());
                let v = std::mem::take(&mut text).trim().to_string();
                if let Some(c) = cur.as_mut() {
                    match name.as_str() {
                        "href" => {
                            // An absolute URL or a path: keep the path
                            let p = url::Url::parse(&v).map(|u| u.path().to_string()).unwrap_or(v);
                            c.path = percent_decode(&p);
                        }
                        "getcontentlength" => c.size = v.parse().unwrap_or(0),
                        "getlastmodified" => {
                            c.mtime = time::OffsetDateTime::parse(
                                &v,
                                &time::format_description::well_known::Rfc2822,
                            )
                            .map(|d| d.unix_timestamp())
                            .unwrap_or(0)
                        }
                        "response" => {
                            if let Some(c) = cur.take() {
                                out.push(c);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;

fn propfind(share: &Share, url: &url::Url) -> Result<Vec<DavEntry>, String> {
    let m = reqwest::Method::from_bytes(b"PROPFIND").expect("method");
    let resp = share
        .auth(crate::sources::http_client().request(m, url.clone()))
        .header("Depth", "1")
        .header(reqwest::header::CONTENT_TYPE, "application/xml")
        .timeout(Duration::from_secs(60))
        .body(PROPFIND_BODY)
        .send()
        .map_err(|e| e.without_url().to_string())?;
    match resp.status().as_u16() {
        207 | 200 => {}
        401 | 403 => return Err("the share refused this user name or password".into()),
        404 => return Err("the share has no such folder".into()),
        st => return Err(format!("the share answered {st}")),
    }
    parse_multistatus(&resp.text().map_err(|e| e.to_string())?)
}

/// Every file under the share, as library entries (key, size, time).
pub(crate) fn list_files(share: &Share) -> Result<Vec<pm_library::scan::RemoteEntry>, String> {
    let base = share.base()?;
    let base_path = percent_decode(base.path());
    let mut out = Vec::new();
    let mut queue = vec![base.clone()];
    let mut seen = std::collections::HashSet::new();
    while let Some(dir) = queue.pop() {
        if !seen.insert(dir.path().to_string()) {
            continue;
        }
        for e in propfind(share, &dir)? {
            let Some(rel) = e.path.strip_prefix(&base_path) else {
                continue;
            };
            let rel = rel.trim_matches('/').to_string();
            if e.dir {
                // The folder itself is listed too
                if !rel.is_empty()
                    && share
                        .file_url(&rel)
                        .map(|u| u.path() != dir.path())
                        .unwrap_or(false)
                {
                    let mut u = share.file_url(&rel)?;
                    let p = format!("{}/", u.path().trim_end_matches('/'));
                    u.set_path(&p);
                    queue.push(u);
                }
            } else if !rel.is_empty() {
                out.push(pm_library::scan::RemoteEntry {
                    path: share.root().join(&rel),
                    size: e.size,
                    mtime: e.mtime,
                });
                if out.len() >= MAX_ENTRIES {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

// ---- reading ----

/// A share's file read with HTTP range requests, for the scanner (tags only).
pub struct RangeReader {
    share: Share,
    url: url::Url,
    len: u64,
    pos: u64,
    buf: Vec<u8>,
    buf_at: u64,
}

impl RangeReader {
    pub fn new(share: Share, url: url::Url, len: u64) -> Self {
        Self {
            share,
            url,
            len,
            pos: 0,
            buf: Vec::new(),
            buf_at: 0,
        }
    }

    fn fill(&mut self, want: usize) -> std::io::Result<()> {
        let end = (self.pos + (want as u64).max(CHUNK))
            .min(self.len)
            .saturating_sub(1);
        let resp = self
            .share
            .auth(crate::sources::http_client().get(self.url.clone()))
            .header(reqwest::header::RANGE, format!("bytes={}-{end}", self.pos))
            .timeout(Duration::from_secs(60))
            .send()
            .map_err(|e| std::io::Error::other(e.without_url().to_string()))?;
        let st = resp.status().as_u16();
        let bytes = resp.bytes().map_err(|e| std::io::Error::other(e.to_string()))?;
        match st {
            206 => {
                self.buf = bytes.to_vec();
                self.buf_at = self.pos;
            }
            // No ranges: the whole file came; keep it
            200 => {
                self.buf = bytes.to_vec();
                self.buf_at = 0;
            }
            st => return Err(std::io::Error::other(format!("the share answered {st}"))),
        }
        Ok(())
    }
}

impl Read for RangeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let inside = self.pos >= self.buf_at && self.pos < self.buf_at + self.buf.len() as u64;
        if !inside {
            self.fill(out.len())?;
        }
        let off = (self.pos - self.buf_at) as usize;
        if off >= self.buf.len() {
            return Ok(0);
        }
        let n = out.len().min(self.buf.len() - off);
        out[..n].copy_from_slice(&self.buf[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let p = match to {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.len as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if p < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the start",
            ));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

impl pm_library::tags::RemoteRead for RangeReader {
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// Scans one share into the library (blocking).
fn scan_share(s: &NodeState, share: &Share) -> Result<pm_library::scan::ScanReport, String> {
    let entries = list_files(share)?;
    let lib = pm_library::Library::open(&s.db_path).map_err(|e| e.to_string())?;
    let scanner = pm_library::Scanner::new(&lib).with_art_dir(&s.art_dir);
    let open = |e: &pm_library::scan::RemoteEntry| -> std::io::Result<Box<dyn pm_library::tags::RemoteRead>> {
        let (_, rel) = split_key(&e.path).ok_or_else(|| std::io::Error::other("bad key"))?;
        let url = share.file_url(&rel).map_err(std::io::Error::other)?;
        Ok(Box::new(RangeReader::new(share.clone(), url, e.size)))
    };
    scanner
        .scan_remote(&share.root(), entries, &open)
        .map_err(|e| e.to_string())
}

/// Opens a library file on a share for a zone: streamed, the credentials kept in the node.
pub(crate) fn open_blocking(
    shared: &Shared,
    key: &Path,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: key.to_string_lossy().into_owned(),
        reason,
    };
    let (id, rel) = split_key(key).ok_or_else(|| fail("not a WebDAV file".into()))?;
    let share = find(shared, &id).ok_or_else(|| fail("that WebDAV share was removed".into()))?;
    let mut url = share.file_url(&rel).map_err(fail)?;
    if !share.user.is_empty() {
        // The HTTP reader takes a URL only: the credentials ride in it, inside the node
        let _ = url.set_username(&share.user);
        let _ = url.set_password(Some(&share.password));
    }
    crate::sources::open_http_stream(url.as_str()).map_err(|e| match e {
        pm_zones::SourceError::Open { reason, .. } => fail(if share.password.is_empty() {
            reason
        } else {
            reason.replace(&share.password, "<password>")
        }),
        other => other,
    })
}

/// A library file on a share or a drive, as it answers: the caller reads the body in chunks,
/// never whole (the download cache, `downloads::local_original`). A local path is not one.
pub(crate) async fn fetch_library_file(s: &NodeState, path: &Path) -> std::io::Result<reqwest::Response> {
    if crate::drives::is_key(path) {
        return crate::drives::fetch_file(s, path).await;
    }
    if !is_key(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not on this machine, a share or a drive",
        ));
    }
    let (id, rel) = split_key(path).ok_or_else(|| std::io::Error::other("bad key"))?;
    let share = find(&s.webdav, &id).ok_or_else(|| std::io::Error::other("that WebDAV share was removed"))?;
    let url = share.file_url(&rel).map_err(std::io::Error::other)?;
    let mut rb = s.http_client().get(url);
    if !share.user.is_empty() {
        rb = rb.basic_auth(&share.user, Some(&share.password));
    }
    let resp = crate::downloads::send_for_cache(rb).await?;
    if !resp.status().is_success() {
        return Err(std::io::Error::other(format!(
            "the share answered {}",
            resp.status()
        )));
    }
    Ok(resp)
}

// ---- routes ----

/// A share as Settings shows it (never the password).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub user: String,
    pub tracks: u64,
    pub bytes: u64,
}

fn view(s: &NodeState, sh: &Share) -> ShareView {
    let (tracks, bytes) = s
        .library
        .lock()
        .expect("library poisoned")
        .folder_totals(&sh.root())
        .unwrap_or((0, 0));
    ShareView {
        id: sh.id.clone(),
        name: sh.name.clone(),
        url: sh.url.clone(),
        user: sh.user.clone(),
        tracks,
        bytes,
    }
}

/// `GET /v1/library/webdav`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<ShareView>> {
    let shares = s.webdav.read().expect("webdav").clone();
    Ok(Json(shares.iter().map(|sh| view(&s, sh)).collect()))
}

#[derive(Clone, Deserialize)]
pub struct NewShare {
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

fn start_scan(s: &Arc<NodeState>, share: Share) {
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || match scan_share(&s2, &share) {
        Ok(r) => {
            tracing::info!(share = %share.name, added = r.files_added, updated = r.files_updated, removed = r.files_removed, errors = r.errors.len(), "WebDAV scan done");
            s2.notify_library();
        }
        Err(e) => tracing::warn!(share = %share.name, "WebDAV scan failed: {e}"),
    });
}

/// `POST /v1/library/webdav {url, name?, user?, password?}`: checks the folder, keeps the share
/// and scans it.
pub(crate) async fn add(State(s): State<Arc<NodeState>>, Json(b): Json<NewShare>) -> ApiResult<ShareView> {
    let u = url::Url::parse(b.url.trim())
        .map_err(|_| ApiError::bad_request("the share's address must be an http(s) URL"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(ApiError::bad_request(
            "the share's address must be an http(s) URL",
        ));
    }
    let share = Share {
        id: crate::tidal::random_urlsafe(6)
            .to_lowercase()
            .replace(['-', '_'], "x"),
        name: b
            .name
            .map(|n| n.trim().chars().take(80).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| u.host_str().unwrap_or("WebDAV").to_string()),
        url: u.to_string(),
        user: b.user.trim().to_string(),
        password: b.password,
    };
    let probe = share.clone();
    tokio::task::spawn_blocking(move || probe.base().and_then(|base| propfind(&probe, &base)))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("WebDAV: {e}")))?;
    let mut shares = s.webdav.read().expect("webdav").clone();
    if shares.iter().any(|x| x.url == share.url && x.user == share.user) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "that share is in the library already",
        ));
    }
    shares.push(share.clone());
    store(&s, &shares)?;
    start_scan(&s, share.clone());
    Ok(Json(view(&s, &share)))
}

/// `POST /v1/library/webdav/{id}/scan`.
pub(crate) async fn rescan(
    State(s): State<Arc<NodeState>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<ShareView> {
    let share = find(&s.webdav, &id).ok_or_else(|| ApiError::not_found("no such share"))?;
    start_scan(&s, share.clone());
    Ok(Json(view(&s, &share)))
}

/// `DELETE /v1/library/webdav/{id}`: forgets the share and its songs in the library.
pub(crate) async fn remove(
    State(s): State<Arc<NodeState>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<serde_json::Value> {
    let mut shares = s.webdav.read().expect("webdav").clone();
    let Some(i) = shares.iter().position(|x| x.id == id) else {
        return Err(ApiError::not_found("no such share"));
    };
    let share = shares.remove(i);
    store(&s, &shares)?;
    let (files, tracks) = s
        .library
        .lock()
        .expect("library poisoned")
        .remove_folder(&share.root())?;
    s.notify_library();
    Ok(Json(
        serde_json::json!({"removed": id, "files": files, "tracks": tracks}),
    ))
}

/// Rescans every share now and then (a share has no change notifications).
pub fn spawn_rescans(s: &Arc<NodeState>) {
    let weak = Arc::downgrade(s);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RESCAN_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { return };
            for sh in s.webdav.read().expect("webdav").clone() {
                start_scan(&s, sh);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistatus_and_keys() {
        let xml = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
          <d:response><d:href>/dav/Music/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
          <d:response><d:href>/dav/Music/Queen%20%26%20Co/01%20Song.flac</d:href><d:propstat><d:prop><d:resourcetype/>
            <d:getcontentlength>47782</d:getcontentlength><d:getlastmodified>Wed, 22 Oct 2026 10:00:00 GMT</d:getlastmodified></d:prop></d:propstat></d:response>
        </d:multistatus>"#;
        let e = parse_multistatus(xml).unwrap();
        assert_eq!(e.len(), 2);
        assert!(e[0].dir);
        assert_eq!(e[1].path, "/dav/Music/Queen & Co/01 Song.flac");
        assert_eq!((e[1].size, e[1].mtime > 1_790_000_000), (47782, true));
        let sh = Share {
            id: "ab12cd".into(),
            name: "NAS".into(),
            url: "https://nas.local/dav/Music".into(),
            user: "me".into(),
            password: ["not", "shown"].join("-"),
        };
        assert_eq!(
            sh.file_url("Queen & Co/01 Song.flac").unwrap().as_str(),
            "https://nas.local/dav/Music/Queen%20&%20Co/01%20Song.flac"
        );
        assert!(sh.file_url("../etc/passwd").is_err());
        let key = sh.root().join("Queen & Co/01 Song.flac");
        assert_eq!(
            split_key(&key),
            Some(("ab12cd".into(), "Queen & Co/01 Song.flac".into()))
        );
        assert!(!format!("{sh:?}").contains("not-shown"));
    }
}
