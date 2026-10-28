//! Google Drive and OneDrive folders as library roots (STEWARD-COMPAT-HA-MA.md §6.4), like a
//! WebDAV share (webdav.rs): the folder is scanned into the library (only each song's tags are
//! fetched, with range requests) and songs play from the drive.
//!
//! **Signing in** is OAuth 2 with PKCE through the project's return page (STEWARD-SHARED-APPS.md):
//! a public client, so only a client id is needed. The client id comes from the node's config
//! (`[drives] google_client_id`, `onedrive_client_id`) or the build (`PM_PROJECT_GOOGLE_CLIENT_ID`,
//! `PM_PROJECT_ONEDRIVE_CLIENT_ID`); without one the drive is offered as "not set up". Google
//! does not redeem a code for a "Web application" client without its secret, so a node may
//! give its own app's secret (`google_client_secret`); it stays in the node's config and is
//! never shipped.
//!
//! **Keys:** a song is `gdrive://<root>/<path>` or `onedrive://<root>/<path>` in the library; the
//! drive's file ids are kept per root (the last listing), for playing.
//! Tokens and file ids stay in the node; none reaches an answer or a log.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

const ACCOUNTS: &str = "drives.accounts";
const ROOTS: &str = "drives.roots";
const INDEX: &str = "drives.index.";
const RESCAN_EVERY: Duration = Duration::from_secs(6 * 3600);
const CHUNK: u64 = 256 * 1024;
const MAX_ENTRIES: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Google,
    Onedrive,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "google" | "gdrive" => Some(Self::Google),
            "onedrive" => Some(Self::Onedrive),
            _ => None,
        }
    }
    fn id(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Onedrive => "onedrive",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Google => "Google Drive",
            Self::Onedrive => "OneDrive",
        }
    }
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Google => "gdrive://",
            Self::Onedrive => "onedrive://",
        }
    }
    fn from_key(key: &Path) -> Option<Self> {
        let k = key.to_str()?;
        [Self::Google, Self::Onedrive]
            .into_iter()
            .find(|p| k.starts_with(p.scheme()))
    }
}

/// A project or node app: the client id (public), and a secret only for a node's own app.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct App {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<set>")
            .finish()
    }
}

/// Where the providers answer (tests point them at mocks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub google_auth: String,
    pub google_token: String,
    pub google_api: String,
    pub ms_login: String,
    pub ms_graph: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            google_auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            google_token: "https://oauth2.googleapis.com/token".into(),
            google_api: "https://www.googleapis.com".into(),
            ms_login: "https://login.microsoftonline.com/common/oauth2/v2.0".into(),
            ms_graph: "https://graph.microsoft.com/v1.0".into(),
        }
    }
}

/// A signed-in drive account. `Debug` never shows the tokens.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub user: Option<String>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("user", &self.user)
            .field("tokens", &"<set>")
            .finish()
    }
}

/// A drive folder that is a library root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Root {
    pub id: String,
    pub provider: Provider,
    pub name: String,
    pub folder_id: String,
}

impl Root {
    pub fn key(&self) -> PathBuf {
        PathBuf::from(format!("{}{}", self.provider.scheme(), self.id))
    }
}

#[derive(Debug, Default)]
pub struct Drives {
    pub apps: HashMap<Provider, App>,
    pub endpoints: Endpoints,
    pub accounts: HashMap<Provider, Account>,
    pub roots: Vec<Root>,
    /// Per root: path inside it → (file id, size), from the last listing.
    pub index: HashMap<String, HashMap<String, (String, u64)>>,
}

pub type Shared = Arc<RwLock<Drives>>;

pub fn load(lib: &pm_library::Library, apps: HashMap<Provider, App>) -> Drives {
    let get = |k: &str| lib.get_setting(k).ok().flatten();
    let roots: Vec<Root> = get(ROOTS)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let index = roots
        .iter()
        .filter_map(|r| {
            Some((
                r.id.clone(),
                serde_json::from_str(&get(&format!("{INDEX}{}", r.id))?).ok()?,
            ))
        })
        .collect();
    Drives {
        apps,
        endpoints: Endpoints::default(),
        accounts: get(ACCOUNTS)
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        roots,
        index,
    }
}

/// The apps from the node's config, else the build's project apps.
pub fn apps_from(google: App, onedrive: App) -> HashMap<Provider, App> {
    let mut m = HashMap::new();
    let pick = |own: App, built: Option<&'static str>| {
        if !own.client_id.trim().is_empty() {
            Some(own)
        } else {
            built.filter(|v| !v.trim().is_empty()).map(|id| App {
                client_id: id.to_string(),
                client_secret: String::new(),
            })
        }
    };
    if let Some(a) = pick(google, option_env!("PM_PROJECT_GOOGLE_CLIENT_ID")) {
        m.insert(Provider::Google, a);
    }
    if let Some(a) = pick(onedrive, option_env!("PM_PROJECT_ONEDRIVE_CLIENT_ID")) {
        m.insert(Provider::Onedrive, a);
    }
    m
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn save(s: &NodeState) -> Result<(), ApiError> {
    let d = s.drives.read().expect("drives");
    let lib = s.library.lock().expect("library poisoned");
    lib.set_setting(ACCOUNTS, &serde_json::to_string(&d.accounts).expect("serializes"))?;
    lib.set_setting(ROOTS, &serde_json::to_string(&d.roots).expect("serializes"))?;
    Ok(())
}

fn upstream(p: Provider, e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("{}: {e}", p.name()))
}

// ---- tokens (blocking: scans and zones run on blocking threads) ----

fn token_url(d: &Drives, p: Provider) -> String {
    match p {
        Provider::Google => d.endpoints.google_token.clone(),
        Provider::Onedrive => format!("{}/token", d.endpoints.ms_login),
    }
}

fn scope(p: Provider) -> &'static str {
    match p {
        Provider::Google => "https://www.googleapis.com/auth/drive.readonly",
        Provider::Onedrive => "Files.Read User.Read offline_access",
    }
}

fn token_post(d: &Drives, p: Provider, form: &[(&str, &str)]) -> Result<Value, String> {
    let app = d.apps.get(&p).ok_or("not set up on this node")?;
    let mut f: Vec<(&str, &str)> = form.to_vec();
    f.push(("client_id", &app.client_id));
    if !app.client_secret.is_empty() {
        f.push(("client_secret", &app.client_secret));
    }
    if p == Provider::Onedrive {
        f.push(("scope", scope(p)));
    }
    let body = {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &f {
            ser.append_pair(k, v);
        }
        ser.finish()
    };
    let resp = crate::sources::http_client()
        .post(token_url(d, p))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| e.without_url().to_string())?;
    let st = resp.status();
    let v: Value = resp.json().map_err(|e| e.to_string())?;
    if !st.is_success() {
        return Err(format!(
            "the sign-in server answered {st}: {}",
            v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or("")
        ));
    }
    Ok(v)
}

/// A valid access token for `p`, refreshed when it is about to expire.
pub(crate) fn access_token(shared: &Shared, s: Option<&NodeState>, p: Provider) -> Result<String, String> {
    {
        let d = shared.read().map_err(|_| "drives")?;
        let a = d.accounts.get(&p).ok_or("not signed in")?;
        if !a.access_token.is_empty() && a.expires_at > now() + 60 {
            return Ok(a.access_token.clone());
        }
    }
    let (refresh, v) = {
        let d = shared.read().map_err(|_| "drives")?;
        let refresh = d.accounts.get(&p).ok_or("not signed in")?.refresh_token.clone();
        let v = token_post(
            &d,
            p,
            &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
        )?;
        (refresh, v)
    };
    let access = v["access_token"].as_str().ok_or("no access token")?.to_string();
    {
        let mut d = shared.write().map_err(|_| "drives")?;
        if let Some(a) = d.accounts.get_mut(&p) {
            a.access_token = access.clone();
            a.expires_at = now() + v["expires_in"].as_i64().unwrap_or(3600);
            // Some servers rotate the refresh token
            a.refresh_token = v["refresh_token"].as_str().map(str::to_string).unwrap_or(refresh);
        }
    }
    if let Some(s) = s {
        let _ = save(s);
    }
    Ok(access)
}

fn api_get(shared: &Shared, p: Provider, url: &str) -> Result<Value, String> {
    let token = access_token(shared, None, p)?;
    let resp = crate::sources::http_client()
        .get(url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(60))
        .send()
        .map_err(|e| e.without_url().to_string())?;
    let st = resp.status();
    if st.as_u16() == 401 {
        return Err("the drive no longer accepts this sign-in: sign in again".into());
    }
    if !st.is_success() {
        return Err(format!("the drive answered {st}"));
    }
    resp.json().map_err(|e| e.to_string())
}

// ---- listing ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub folder: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: i64,
}

fn rfc3339(v: &Value) -> i64 {
    v.as_str()
        .and_then(|t| time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok())
        .map(|d| d.unix_timestamp())
        .unwrap_or(0)
}

/// One folder's children (all pages).
pub(crate) fn children(shared: &Shared, p: Provider, folder: &str) -> Result<Vec<Item>, String> {
    let ep = shared.read().map_err(|_| "drives")?.endpoints.clone();
    let mut out = Vec::new();
    match p {
        Provider::Google => {
            let mut page: Option<String> = None;
            loop {
                let mut u = url::Url::parse(&format!("{}/drive/v3/files", ep.google_api))
                    .map_err(|e| e.to_string())?;
                u.query_pairs_mut()
                    .append_pair(
                        "q",
                        &format!("'{}' in parents and trashed = false", folder.replace('\'', "")),
                    )
                    .append_pair(
                        "fields",
                        "nextPageToken,files(id,name,mimeType,size,modifiedTime)",
                    )
                    .append_pair("pageSize", "1000");
                if let Some(t) = &page {
                    u.query_pairs_mut().append_pair("pageToken", t);
                }
                let v = api_get(shared, p, u.as_str())?;
                for f in v["files"].as_array().cloned().unwrap_or_default() {
                    out.push(Item {
                        id: f["id"].as_str().unwrap_or_default().to_string(),
                        name: f["name"].as_str().unwrap_or_default().to_string(),
                        folder: f["mimeType"].as_str() == Some("application/vnd.google-apps.folder"),
                        size: f["size"]
                            .as_str()
                            .and_then(|n| n.parse().ok())
                            .or(f["size"].as_u64())
                            .unwrap_or(0),
                        mtime: rfc3339(&f["modifiedTime"]),
                    });
                }
                page = v["nextPageToken"].as_str().map(str::to_string);
                if page.is_none() {
                    break;
                }
            }
        }
        Provider::Onedrive => {
            let mut next = Some(format!(
                "{}/me/drive/items/{}/children?$select=id,name,size,file,folder,lastModifiedDateTime&$top=999",
                ep.ms_graph,
                url::form_urlencoded::byte_serialize(folder.as_bytes()).collect::<String>()
            ));
            while let Some(u) = next.take() {
                // Only follow pages on the same API
                if !u.starts_with(&ep.ms_graph) {
                    break;
                }
                let v = api_get(shared, p, &u)?;
                for f in v["value"].as_array().cloned().unwrap_or_default() {
                    out.push(Item {
                        id: f["id"].as_str().unwrap_or_default().to_string(),
                        name: f["name"].as_str().unwrap_or_default().to_string(),
                        folder: f["folder"].is_object(),
                        size: f["size"].as_u64().unwrap_or(0),
                        mtime: rfc3339(&f["lastModifiedDateTime"]),
                    });
                }
                next = v["@odata.nextLink"].as_str().map(str::to_string);
            }
        }
    }
    out.retain(|i| !i.id.is_empty() && !i.name.is_empty() && !i.name.contains('/'));
    Ok(out)
}

/// Every file under a root: (path inside it, item).
fn list_all(shared: &Shared, root: &Root) -> Result<Vec<(String, Item)>, String> {
    let mut out = Vec::new();
    let mut queue = vec![(String::new(), root.folder_id.clone())];
    let mut seen = std::collections::HashSet::new();
    while let Some((prefix, id)) = queue.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        for it in children(shared, root.provider, &id)? {
            let path = if prefix.is_empty() {
                it.name.clone()
            } else {
                format!("{prefix}/{}", it.name)
            };
            if it.folder {
                queue.push((path, it.id));
            } else {
                out.push((path, it));
                if out.len() >= MAX_ENTRIES {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

fn content_url(ep: &Endpoints, p: Provider, file_id: &str) -> String {
    let id: String = url::form_urlencoded::byte_serialize(file_id.as_bytes()).collect();
    match p {
        Provider::Google => format!("{}/drive/v3/files/{id}?alt=media", ep.google_api),
        Provider::Onedrive => format!("{}/me/drive/items/{id}/content", ep.ms_graph),
    }
}

/// A drive file read with range requests (for the scanner).
pub struct RangeReader {
    url: String,
    token: String,
    len: u64,
    pos: u64,
    buf: Vec<u8>,
    buf_at: u64,
}

impl Read for RangeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let inside = self.pos >= self.buf_at && self.pos < self.buf_at + self.buf.len() as u64;
        if !inside {
            let end = (self.pos + (out.len() as u64).max(CHUNK))
                .min(self.len)
                .saturating_sub(1);
            let resp = crate::sources::http_client()
                .get(&self.url)
                .bearer_auth(&self.token)
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
                200 => {
                    self.buf = bytes.to_vec();
                    self.buf_at = 0;
                }
                st => return Err(std::io::Error::other(format!("the drive answered {st}"))),
            }
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

fn scan_root(s: &NodeState, root: &Root) -> Result<pm_library::scan::ScanReport, String> {
    let files = list_all(&s.drives, root)?;
    let index: HashMap<String, (String, u64)> = files
        .iter()
        .map(|(p, it)| (p.clone(), (it.id.clone(), it.size)))
        .collect();
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(
            &format!("{INDEX}{}", root.id),
            &serde_json::to_string(&index).expect("serializes"),
        )
        .map_err(|e| e.to_string())?;
    s.drives
        .write()
        .map_err(|_| "drives")?
        .index
        .insert(root.id.clone(), index.clone());
    let entries = files
        .iter()
        .map(|(p, it)| pm_library::scan::RemoteEntry {
            path: root.key().join(p),
            size: it.size,
            mtime: it.mtime,
        })
        .collect();
    let token = access_token(&s.drives, Some(s), root.provider)?;
    let ep = s.drives.read().map_err(|_| "drives")?.endpoints.clone();
    let lib = pm_library::Library::open(&s.db_path).map_err(|e| e.to_string())?;
    let scanner = pm_library::Scanner::new(&lib).with_art_dir(&s.art_dir);
    let open = |e: &pm_library::scan::RemoteEntry| -> std::io::Result<Box<dyn pm_library::tags::RemoteRead>> {
        let (_, rel) = split_key(&e.path).ok_or_else(|| std::io::Error::other("bad key"))?;
        let (id, size) = index
            .get(&rel)
            .cloned()
            .ok_or_else(|| std::io::Error::other("not in the listing"))?;
        Ok(Box::new(RangeReader {
            url: content_url(&ep, root.provider, &id),
            token: token.clone(),
            len: size,
            pos: 0,
            buf: Vec::new(),
            buf_at: 0,
        }))
    };
    scanner
        .scan_remote(&root.key(), entries, &open)
        .map_err(|e| e.to_string())
}

/// A library key's root id and path inside it.
pub fn split_key(key: &Path) -> Option<(String, String)> {
    let p = Provider::from_key(key)?;
    let k = key.to_str()?.strip_prefix(p.scheme())?;
    let (id, rel) = k.split_once('/')?;
    Some((id.to_string(), rel.to_string()))
}

pub fn is_key(key: &Path) -> bool {
    Provider::from_key(key).is_some()
}

/// The content URL and bearer token for a library key.
fn locate(shared: &Shared, key: &Path) -> Result<(String, String, Provider), String> {
    let p = Provider::from_key(key).ok_or("not a drive file")?;
    let (root, rel) = split_key(key).ok_or("bad key")?;
    let (url, ok) = {
        let d = shared.read().map_err(|_| "drives")?;
        if !d.roots.iter().any(|r| r.id == root) {
            return Err("that drive folder was removed".into());
        }
        let (id, _) = d
            .index
            .get(&root)
            .and_then(|i| i.get(&rel))
            .cloned()
            .ok_or("not on the drive any more: rescan")?;
        (content_url(&d.endpoints, p, &id), true)
    };
    let _ = ok;
    let token = access_token(shared, None, p)?;
    Ok((url, token, p))
}

/// Opens a library file on a drive for a zone: streamed with the token in a header.
pub(crate) fn open_blocking(
    shared: &Shared,
    key: &Path,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: key.to_string_lossy().into_owned(),
        reason,
    };
    let (url, token, _) = locate(shared, key).map_err(fail)?;
    let ext = key
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let ext: Option<&'static str> = match ext.as_deref() {
        Some("flac") => Some("flac"),
        Some("mp3") => Some("mp3"),
        Some("m4a") | Some("alac") => Some("m4a"),
        Some("ogg") | Some("oga") => Some("ogg"),
        Some("wav") | Some("wave") => Some("wav"),
        Some("aif") | Some("aiff") => Some("aiff"),
        _ => None,
    };
    crate::sources::open_http_stream_with(&url, &[("Authorization".into(), format!("Bearer {token}"))], ext)
        .map_err(|e| match e {
            pm_zones::SourceError::Open { reason, .. } => fail(reason.replace(&token, "<token>")),
            other => other,
        })
}

/// A library file on a drive, as the drive answers: the caller reads the body in chunks.
pub(crate) async fn fetch_file(s: &NodeState, key: &Path) -> std::io::Result<reqwest::Response> {
    let shared = s.drives.clone();
    let key2 = key.to_path_buf();
    let (url, token, _) = tokio::task::spawn_blocking(move || locate(&shared, &key2))
        .await
        .map_err(std::io::Error::other)?
        .map_err(std::io::Error::other)?;
    let resp = crate::downloads::send_for_cache(s.http_client().get(url).bearer_auth(token)).await?;
    if !resp.status().is_success() {
        return Err(std::io::Error::other(format!(
            "the drive answered {}",
            resp.status()
        )));
    }
    Ok(resp)
}

// ---- signing in ----

/// Pending sign-ins: nonce → (provider, PKCE verifier, redirect URI).
type Pending = Mutex<HashMap<String, (Provider, String, String)>>;

fn pending() -> &'static Pending {
    static P: std::sync::OnceLock<Pending> = std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

fn provider_of(name: &str) -> Result<Provider, ApiError> {
    Provider::parse(name).ok_or_else(|| ApiError::not_found("no such drive (google, onedrive)"))
}

/// `POST /v1/library/drives/{provider}/login`: the provider's sign-in page.
pub(crate) async fn login(
    State(s): State<Arc<NodeState>>,
    AxPath(provider): AxPath<String>,
    headers: axum::http::HeaderMap,
) -> ApiResult<crate::tidal::LoginUrl> {
    use base64::Engine;
    use sha2::Digest;
    let p = provider_of(&provider)?;
    let d = s.drives.read().expect("drives");
    let app = d.apps.get(&p).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            format!(
                "{} is not set up on this node: add [drives] {}_client_id",
                p.name(),
                p.id()
            ),
        )
    })?;
    let verifier = crate::tidal::random_urlsafe(48);
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let nonce = crate::tidal::random_urlsafe(18);
    let callback = format!(
        "{}/drives/{}/callback",
        crate::tidal::external_base(&s, &headers),
        p.id()
    );
    // A node's own app with its secret returns to the node; the project app via the return page
    let (redirect, state) = if app.client_secret.is_empty() {
        (
            s.return_page(),
            crate::project_apps::return_state(&nonce, &callback),
        )
    } else {
        (callback.clone(), nonce.clone())
    };
    pending()
        .lock()
        .expect("pending")
        .insert(nonce, (p, verifier, redirect.clone()));
    let base = match p {
        Provider::Google => d.endpoints.google_auth.clone(),
        Provider::Onedrive => format!("{}/authorize", d.endpoints.ms_login),
    };
    let mut u = url::Url::parse(&base).map_err(|e| upstream(p, e))?;
    u.query_pairs_mut()
        .append_pair("client_id", &app.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", scope(p))
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge)
        .append_pair("state", &state);
    if p == Provider::Google {
        u.query_pairs_mut()
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
    }
    Ok(Json(crate::tidal::LoginUrl { url: u.to_string() }))
}

#[derive(Debug, Deserialize, Default)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// `GET /drives/{provider}/callback`: the provider (via the return page) sends the browser here.
pub(crate) async fn callback(
    State(s): State<Arc<NodeState>>,
    AxPath(provider): AxPath<String>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let back = |r: Result<(), ApiError>| match r {
        Ok(()) => Redirect::to("/settings/library?drive=ok").into_response(),
        Err(e) => Redirect::to(&format!(
            "/settings/library?drive=error&message={}",
            url::form_urlencoded::byte_serialize(e.message.as_bytes()).collect::<String>()
        ))
        .into_response(),
    };
    back(finish_login(&s, &provider, q).await)
}

async fn finish_login(s: &Arc<NodeState>, provider: &str, q: CallbackQuery) -> Result<(), ApiError> {
    let p = provider_of(provider)?;
    if let Some(e) = q.error {
        return Err(ApiError::bad_request(format!(
            "{} refused the sign-in: {e}",
            p.name()
        )));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(ApiError::bad_request("missing code or state"));
    };
    let nonce = crate::project_apps::state_nonce(&state).to_string();
    let (pp, verifier, redirect) = pending()
        .lock()
        .expect("pending")
        .remove(&nonce)
        .ok_or_else(|| ApiError::bad_request("unknown or expired sign-in: start again"))?;
    if pp != p {
        return Err(ApiError::bad_request("that sign-in was for another drive"));
    }
    let s2 = s.clone();
    let account = tokio::task::spawn_blocking(move || -> Result<Account, String> {
        let d = s2.drives.read().map_err(|_| "drives")?;
        let v = token_post(
            &d,
            p,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect),
                ("code_verifier", &verifier),
            ],
        )?;
        Ok(Account {
            refresh_token: v["refresh_token"]
                .as_str()
                .ok_or("no refresh token (offline access was not granted)")?
                .to_string(),
            access_token: v["access_token"].as_str().unwrap_or_default().to_string(),
            expires_at: now() + v["expires_in"].as_i64().unwrap_or(3600),
            user: None,
        })
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| upstream(p, e))?;
    s.drives.write().expect("drives").accounts.insert(p, account);
    // Who it is, for Settings
    let s3 = s.clone();
    let user = tokio::task::spawn_blocking(move || {
        let ep = s3.drives.read().ok()?.endpoints.clone();
        let v = match p {
            Provider::Google => api_get(
                &s3.drives,
                p,
                &format!(
                    "{}/drive/v3/about?fields=user(emailAddress,displayName)",
                    ep.google_api
                ),
            )
            .ok()?,
            Provider::Onedrive => api_get(&s3.drives, p, &format!("{}/me", ep.ms_graph)).ok()?,
        };
        v["user"]["emailAddress"]
            .as_str()
            .or(v["userPrincipalName"].as_str())
            .or(v["mail"].as_str())
            .map(str::to_string)
    })
    .await
    .ok()
    .flatten();
    if let Some(a) = s.drives.write().expect("drives").accounts.get_mut(&p) {
        a.user = user;
    }
    save(s)?;
    tracing::info!(drive = p.name(), "signed in");
    Ok(())
}

// ---- routes ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    /// An app (client id) is set up for it.
    pub configured: bool,
    pub connected: bool,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootView {
    pub id: String,
    pub provider: Provider,
    pub name: String,
    pub tracks: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrivesView {
    pub providers: Vec<ProviderView>,
    pub roots: Vec<RootView>,
}

/// `GET /v1/library/drives`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<DrivesView> {
    let d = s.drives.read().expect("drives");
    let providers = [Provider::Google, Provider::Onedrive]
        .into_iter()
        .map(|p| ProviderView {
            id: p.id().into(),
            name: p.name().into(),
            configured: d.apps.contains_key(&p),
            connected: d.accounts.contains_key(&p),
            user: d.accounts.get(&p).and_then(|a| a.user.clone()),
        })
        .collect();
    let roots = d
        .roots
        .iter()
        .map(|r| {
            let (tracks, bytes) = s
                .library
                .lock()
                .expect("library poisoned")
                .folder_totals(&r.key())
                .unwrap_or((0, 0));
            RootView {
                id: r.id.clone(),
                provider: r.provider,
                name: r.name.clone(),
                tracks,
                bytes,
            }
        })
        .collect();
    Ok(Json(DrivesView { providers, roots }))
}

#[derive(Debug, Deserialize)]
pub struct FolderQuery {
    #[serde(default)]
    pub parent: Option<String>,
}

/// `GET /v1/library/drives/{provider}/folders?parent=`: folders to pick from (`root` first).
pub(crate) async fn folders(
    State(s): State<Arc<NodeState>>,
    AxPath(provider): AxPath<String>,
    Query(q): Query<FolderQuery>,
) -> ApiResult<Vec<Item>> {
    let p = provider_of(&provider)?;
    let parent = q.parent.unwrap_or_else(|| "root".into());
    let shared = s.drives.clone();
    let items = tokio::task::spawn_blocking(move || children(&shared, p, &parent))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| upstream(p, e))?;
    Ok(Json(items.into_iter().filter(|i| i.folder).collect()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewRoot {
    pub provider: String,
    pub folder_id: String,
    #[serde(default)]
    pub name: Option<String>,
}

fn start_scan(s: &Arc<NodeState>, root: Root) {
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || match scan_root(&s2, &root) {
        Ok(r) => {
            tracing::info!(drive = root.provider.name(), folder = %root.name, added = r.files_added, removed = r.files_removed, errors = r.errors.len(), "drive scan done");
            s2.notify_library();
        }
        Err(e) => tracing::warn!(drive = root.provider.name(), folder = %root.name, "drive scan failed: {e}"),
    });
}

/// `POST /v1/library/drives {provider, folder_id, name?}`: a drive folder becomes a library root.
pub(crate) async fn add(State(s): State<Arc<NodeState>>, Json(b): Json<NewRoot>) -> ApiResult<RootView> {
    let p = provider_of(&b.provider)?;
    if !s.drives.read().expect("drives").accounts.contains_key(&p) {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            format!("sign in to {} first", p.name()),
        ));
    }
    let folder = b.folder_id.trim().to_string();
    if folder.is_empty() || folder.len() > 200 {
        return Err(ApiError::bad_request("folder_id is required"));
    }
    let root = Root {
        id: crate::tidal::random_urlsafe(6)
            .to_lowercase()
            .replace(['-', '_'], "x"),
        provider: p,
        name: b
            .name
            .map(|n| n.trim().chars().take(80).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| p.name().to_string()),
        folder_id: folder,
    };
    {
        let mut d = s.drives.write().expect("drives");
        if d.roots
            .iter()
            .any(|r| r.provider == p && r.folder_id == root.folder_id)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "that folder is in the library already",
            ));
        }
        d.roots.push(root.clone());
    }
    save(&s)?;
    start_scan(&s, root.clone());
    Ok(Json(RootView {
        id: root.id,
        provider: p,
        name: root.name,
        tracks: 0,
        bytes: 0,
    }))
}

/// `POST /v1/library/drives/roots/{id}/scan`.
pub(crate) async fn rescan(
    State(s): State<Arc<NodeState>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<serde_json::Value> {
    let root = s
        .drives
        .read()
        .expect("drives")
        .roots
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("no such folder"))?;
    start_scan(&s, root);
    Ok(Json(serde_json::json!({"scanning": id})))
}

/// `DELETE /v1/library/drives/roots/{id}`: the folder and its songs leave the library.
pub(crate) async fn remove(
    State(s): State<Arc<NodeState>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<serde_json::Value> {
    let root = {
        let mut d = s.drives.write().expect("drives");
        let i = d
            .roots
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| ApiError::not_found("no such folder"))?;
        d.index.remove(&id);
        d.roots.remove(i)
    };
    save(&s)?;
    let lib = s.library.lock().expect("library poisoned");
    let _ = lib.delete_setting(&format!("{INDEX}{id}"));
    let (files, tracks) = lib.remove_folder(&root.key())?;
    drop(lib);
    s.notify_library();
    Ok(Json(
        serde_json::json!({"removed": id, "files": files, "tracks": tracks}),
    ))
}

/// `DELETE /v1/library/drives/{provider}/account`: signs out (its folders stay until removed).
pub(crate) async fn sign_out(
    State(s): State<Arc<NodeState>>,
    AxPath(provider): AxPath<String>,
) -> ApiResult<serde_json::Value> {
    let p = provider_of(&provider)?;
    s.drives.write().expect("drives").accounts.remove(&p);
    save(&s)?;
    Ok(Json(serde_json::json!({"signed_out": p.id()})))
}

/// Rescans every drive folder now and then.
pub fn spawn_rescans(s: &Arc<NodeState>) {
    let weak = Arc::downgrade(s);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RESCAN_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { return };
            let roots = s.drives.read().expect("drives").roots.clone();
            for r in roots {
                start_scan(&s, r);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_secrets() {
        let r = Root {
            id: "ab12".into(),
            provider: Provider::Onedrive,
            name: "Music".into(),
            folder_id: "X!1".into(),
        };
        let k = r.key().join("Queen/01 Song.flac");
        assert_eq!(k.to_str(), Some("onedrive://ab12/Queen/01 Song.flac"));
        assert_eq!(split_key(&k), Some(("ab12".into(), "Queen/01 Song.flac".into())));
        assert!(is_key(&k) && !is_key(Path::new("/music/a.flac")));
        let a = Account {
            refresh_token: "R3FRESH".into(),
            access_token: "ACC".into(),
            ..Default::default()
        };
        assert!(!format!("{a:?}").contains("R3FRESH"));
        let app = App {
            client_id: "id".into(),
            client_secret: "SEC".into(),
        };
        assert!(!format!("{app:?}").contains("SEC"));
        assert_eq!(
            content_url(&Endpoints::default(), Provider::Google, "f1"),
            "https://www.googleapis.com/drive/v3/files/f1?alt=media"
        );
    }
}
