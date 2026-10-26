//! The node's side of the sources vault (`crates/pubky-music-specs/VAULT.md`): its own client key,
//! and reading and writing sign-ins on the person's homeserver through their session.
//!
//! Everything is encrypted here before it leaves the node ([`pubky_music_specs::vault::seal`]);
//! the homeserver only ever sees envelopes. Which clients a blob is wrapped to is the caller's
//! choice: the trusted entries of the device list.

use crate::homeserver::{is_not_found, HomeserverError};
use pubky::PubkySession;
use pubky_music_specs::vault::{
    self as spec, ClientEntry, ClientKey, ClientProofData, ClientSecret, DataKey, Envelope, Index, Proof,
    Revoked, SignIn, VaultError,
};
use std::path::Path;
use std::time::Duration;

/// What can go wrong reading or writing the vault.
#[derive(Debug, thiserror::Error)]
pub enum VaultStoreError {
    #[error(transparent)]
    Homeserver(#[from] HomeserverError),
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error("{0} is not a service id")]
    BadService(String),
    #[error("the sign-in for {0} was made on a device this node can't read for; sign out of it there, or delete it, first")]
    NotOurs(String),
    #[error("this node was signed out of your streaming sign-ins on another device; to add it back, approve it again in Pubky Ring and give it a new vault key")]
    Revoked,
    #[error("this sign-in can't prove itself to other devices; sign the node in again with Pubky Ring")]
    NoGrant,
    #[error("another device is changing {0}; try again in a moment")]
    Busy(String),
    #[error("the service refused the refresh: {0}")]
    Refresh(String),
    #[error("this node's Pubky sign-in doesn't include your streaming sign-ins yet; sign it in again with Pubky Ring")]
    NeedsVaultGrant,
    #[error("the node's record of signed-out devices: {0}")]
    Local(#[from] std::io::Error),
    /// The service refreshed the tokens but they couldn't be written to the vault. `sign_in`
    /// holds them: after a rotation they are the only ones that still work, so the caller uses
    /// them and writes them again later.
    #[error("the new {service} tokens couldn't be saved to the vault yet: {reason}")]
    Unsaved {
        service: String,
        sign_in: Box<SignIn>,
        reason: String,
    },
}

impl VaultStoreError {
    /// The homeserver's HTTP status, where the error is one.
    fn http_status(&self) -> Option<reqwest::StatusCode> {
        match self {
            Self::Homeserver(HomeserverError::Pubky(e)) => status(e),
            _ => None,
        }
    }

    /// Worth trying again shortly: the homeserver or the network, not the data.
    fn transient(&self) -> bool {
        matches!(self, Self::Busy(_) | Self::Homeserver(_))
    }
}

impl From<pubky::Error> for VaultStoreError {
    fn from(e: pubky::Error) -> Self {
        Self::Homeserver(HomeserverError::Pubky(e))
    }
}

type Result<T> = std::result::Result<T, VaultStoreError>;

/// The node's X25519 vault key, from `path` (32 raw bytes, mode 0600), made on first use.
/// The secret is never logged; the file is the only copy.
pub fn load_or_create_client_secret(path: &Path) -> std::io::Result<ClientSecret> {
    let read = |path: &Path| -> std::io::Result<ClientSecret> {
        let b = std::fs::read(path)?;
        let b: [u8; 32] = b.try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a vault key (32 bytes)", path.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(ClientSecret::from_bytes(b))
    };
    if path.exists() {
        return read(path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let secret = ClientSecret::generate();
    // Written whole to a file of its own, then linked into place, so another process never
    // reads a half-written key
    let tmp = tmp_beside(path);
    write_private(&tmp, &secret.to_bytes())?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(secret),
        // Another process made it first: use theirs
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(path),
        // No hard links here (vfat, exFAT): create it in place, and let a process that loses
        // the race wait for the winner's whole key
        Err(_) => create_in_place(path, &secret, read),
    }
}

fn create_in_place(
    path: &Path,
    secret: &ClientSecret,
    read: impl Fn(&Path) -> std::io::Result<ClientSecret>,
) -> std::io::Result<ClientSecret> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    match o.open(path) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(&secret.to_bytes())?;
            f.sync_all()?;
            Ok(ClientSecret::from_bytes(secret.to_bytes()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            for _ in 0..40 {
                if std::fs::metadata(path).is_ok_and(|m| m.len() == 32) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            read(path)
        }
        Err(e) => Err(e),
    }
}

/// A file name next to `path` that no other writer, in this process or another, uses.
fn tmp_beside(path: &Path) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}.{n}", std::process::id()));
    path.with_file_name(name)
}

/// Write `bytes` to `path` (mode 0600 on unix), replacing what is there, and sync it.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    use std::io::Write;
    f.write_all(bytes)?;
    f.sync_all()
}

/// The most signed-out clients a node remembers from device lists (its own sign-outs are kept
/// apart and never dropped).
const MAX_LOCAL_REVOKED: usize = 4096;

/// A node's own record of signed-out clients (`<data>/vault/revoked.json`).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct LocalRevoked {
    /// Sign-outs this node made: never dropped.
    #[serde(default)]
    made: Vec<Revoked>,
    /// Sign-outs it saw in device lists, up to [`MAX_LOCAL_REVOKED`]: a flood of made-up ones
    /// only fills what is left.
    #[serde(default)]
    seen: Vec<Revoked>,
}

impl LocalRevoked {
    fn all(&self) -> impl Iterator<Item = &Revoked> {
        self.made.iter().chain(&self.seen)
    }
}

/// One read-modify-write of the file at a time in this process.
fn revoked_file_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|p| p.into_inner())
}

fn read_local_revoked(path: &Path) -> std::io::Result<LocalRevoked> {
    match std::fs::read(path) {
        Ok(b) => {
            let bad = |e: serde_json::Error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            };
            // The first form of the file: a plain list of seen records
            if b.trim_ascii_start().starts_with(b"[") {
                return Ok(LocalRevoked {
                    made: Vec::new(),
                    seen: serde_json::from_slice(&b).map_err(bad)?,
                });
            }
            serde_json::from_slice(&b).map_err(bad)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LocalRevoked::default()),
        Err(e) => Err(e),
    }
}

/// Add the well-formed records of `revoked` to the seen records in the file at `path`, except
/// those naming this client (`me`, `my_grant`: a list that signs this node out is followed while
/// it says so, never remembered). Returns every record the file holds.
pub fn remember_revoked(
    path: &Path,
    revoked: &[Revoked],
    me: &ClientKey,
    my_grant: Option<&str>,
) -> std::io::Result<Vec<Revoked>> {
    let _l = revoked_file_lock();
    let mut local = read_local_revoked(path)?;
    let mut added = false;
    for r in revoked {
        if local.seen.len() >= MAX_LOCAL_REVOKED {
            break;
        }
        if r.is_well_formed() && !r.names(me, my_grant) && !local.all().any(|x| x.same_client(r)) {
            local.seen.push(r.clone());
            added = true;
        }
    }
    if added {
        write_local_revoked(path, &local)?;
    }
    Ok(local.all().cloned().collect())
}

/// Record a sign-out this node made, for good. An error means it wasn't recorded.
pub fn remember_made(path: &Path, r: &Revoked) -> std::io::Result<()> {
    let _l = revoked_file_lock();
    let mut local = read_local_revoked(path)?;
    if local.made.iter().any(|x| x.same_client(r)) {
        return Ok(());
    }
    if local.made.len() >= MAX_LOCAL_REVOKED {
        return Err(std::io::Error::other("too many sign-outs recorded on this node"));
    }
    local.made.push(r.clone());
    write_local_revoked(path, &local)
}

/// Replace the node's file of signed-out clients (written whole, then renamed into place).
fn write_local_revoked(path: &Path, list: &LocalRevoked) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
    let tmp = tmp_beside(path);
    write_private(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

/// `index.json` as bytes within [`spec::MAX_INDEX_BYTES`]: when too large, signed-out records
/// that name no listed entry go first (they change nobody's trust).
fn index_body(ix: &mut Index) -> Result<Vec<u8>> {
    let enc =
        |ix: &Index| serde_json::to_vec(ix).map_err(|_| VaultStoreError::from(VaultError::Format("index")));
    let mut body = enc(ix)?;
    if body.len() <= spec::MAX_INDEX_BYTES {
        return Ok(body);
    }
    let named: Vec<(ClientKey, Option<String>)> = ix
        .clients
        .iter()
        .map(|e| {
            (
                e.client_key,
                pubky::GrantClaims::decode(&e.proof.grant)
                    .ok()
                    .map(|c| c.jti.to_string()),
            )
        })
        .collect();
    ix.revoked
        .retain(|r| r.is_well_formed() && named.iter().any(|(k, g)| r.names(k, g.as_deref())));
    body = enc(ix)?;
    if body.len() > spec::MAX_INDEX_BYTES {
        return Err(VaultError::TooLarge.into());
    }
    Ok(body)
}

/// A blob opened by this node.
#[derive(Debug)]
pub struct Opened {
    /// The sign-in.
    pub sign_in: SignIn,
    /// The envelope's epoch.
    pub epoch: u64,
    /// Who it is wrapped to now.
    pub recipients: Vec<ClientKey>,
    data_key: DataKey,
}

/// The vault of the session's owner, as this node (holding `secret`) sees it.
pub struct Vault<'a> {
    session: &'a PubkySession,
    owner: String,
    secret: &'a ClientSecret,
    /// Coordinate refreshes with the lease file even where locks work (tests of the fallback).
    lease_only: bool,
    /// The device kind this client signs into its proof (`node`, `desktop`, …).
    device: Option<&'static str>,
    /// This client's own record of signed-out clients (VAULT.md, "Revocation is remembered").
    revocations: Option<&'a Path>,
}

impl<'a> Vault<'a> {
    pub fn new(session: &'a PubkySession, secret: &'a ClientSecret) -> Self {
        Self {
            session,
            owner: session.public_key().z32(),
            secret,
            lease_only: false,
            device: None,
            revocations: None,
        }
    }

    /// Remember every signed-out client in the file at `path`, and keep trusting none of them
    /// even when a later `index.json` no longer lists them. Every real client sets this: the
    /// device list is writable by the clients it lists, the signed-out ones too.
    pub fn remember_revocations(mut self, path: &'a Path) -> Self {
        self.revocations = Some(path);
        self
    }

    /// This client's grant id, when its session is a grant.
    async fn my_grant(&self) -> Option<String> {
        match self.session.as_grant() {
            Some(g) => Some(g.grant_id().await.to_string()),
            None => None,
        }
    }

    /// Record the well-formed records of `revoked` in this client's own list (not those naming
    /// this client), and return every record that list holds.
    pub async fn remember(&self, revoked: &[Revoked]) -> Result<Vec<Revoked>> {
        let Some(path) = self.revocations else {
            return Ok(Vec::new());
        };
        let me = self.client_key();
        let mine = self.my_grant().await;
        Ok(remember_revoked(path, revoked, &me, mine.as_deref())?)
    }

    /// Record a sign-out this client made, for good (nothing to do without a local record).
    pub fn remember_made(&self, r: &Revoked) -> Result<()> {
        match self.revocations {
            Some(path) => Ok(remember_made(path, r)?),
            None => Ok(()),
        }
    }

    /// Coordinate refreshes with the lease file only, as on a homeserver without locks (tests).
    pub fn lease_only(mut self) -> Self {
        self.lease_only = true;
        self
    }

    /// The kind of device this client is, signed into its proof: `node`, `desktop`, `android`,
    /// `ios`, `browser`.
    pub fn device(mut self, kind: &'static str) -> Self {
        self.device = Some(kind);
        self
    }

    /// The owner's z32 key (bound into every envelope).
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// This node's X25519 public key.
    pub fn client_key(&self) -> ClientKey {
        self.secret.public()
    }

    /// A vault file's bytes, `None` when there is none; refused unread past `max` bytes.
    pub(crate) async fn read_bytes(&self, path: &str, max: usize) -> Result<Option<Vec<u8>>> {
        let mut resp = match self.session.storage().get(path).await {
            Ok(r) => r,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if resp.content_length().is_some_and(|n| n as usize > max) {
            return Err(VaultError::TooLarge.into());
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| HomeserverError::Transport(e.to_string()))?
        {
            if out.len() + chunk.len() > max {
                return Err(VaultError::TooLarge.into());
            }
            out.extend_from_slice(&chunk);
        }
        Ok(Some(out))
    }

    fn blob_path(service: &str) -> Result<String> {
        spec::blob_path(service).ok_or_else(|| VaultStoreError::BadService(service.to_string()))
    }

    /// The envelope of a service's sign-in, as stored.
    pub async fn envelope(&self, service: &str) -> Result<Option<Envelope>> {
        let path = Self::blob_path(service)?;
        match self.read_bytes(&path, spec::MAX_BLOB_BYTES).await? {
            Some(b) => Ok(Some(Envelope::parse(&b)?)),
            None => Ok(None),
        }
    }

    /// A service's sign-in, decrypted; `None` when there is none.
    /// [`VaultError::NotARecipient`] when this node isn't (yet) one of its clients.
    pub async fn read(&self, service: &str) -> Result<Option<Opened>> {
        let path = Self::blob_path(service)?;
        let Some(env) = self.envelope(service).await? else {
            return Ok(None);
        };
        let (data_key, sign_in) = spec::open_sign_in(&self.owner, &path, &env, self.secret)?;
        Ok(Some(Opened {
            sign_in,
            epoch: env.epoch,
            recipients: env.recipients.iter().map(|r| r.client_key).collect(),
            data_key,
        }))
    }

    /// Write a service's sign-in, wrapped to this node and to `others`. Returns its revision.
    ///
    /// - A new sign-in starts at epoch 1 with a new data key.
    /// - Rewriting one this node can open keeps its data key and epoch while nobody is dropped.
    ///   When a current recipient is missing from the new set, the key rotates (epoch + 1), so a
    ///   dropped client can't read what is written from now on.
    /// - One this node can't open is not overwritten ([`VaultStoreError::NotOurs`]): that would
    ///   lose a sign-in made elsewhere.
    pub async fn write(&self, service: &str, sign_in: &SignIn, others: &[ClientKey]) -> Result<u64> {
        let path = Self::blob_path(service)?;
        let held = self.lock(&path).await?;
        let r = self.write_held(&held, service, &path, sign_in, others).await;
        self.unlock(held).await;
        r
    }

    /// [`Vault::write`] under a lock already held on `path`.
    async fn write_held(
        &self,
        held: &Locked,
        service: &str,
        path: &str,
        sign_in: &SignIn,
        others: &[ClientKey],
    ) -> Result<u64> {
        let me = self.client_key();
        let mut recipients = vec![me];
        // Read under the blob's lock: a device signed out meanwhile is in the index already, so
        // it is dropped here (and the key rotates past it) whatever list the caller had
        let out = revoked_keys(&self.index().await?);
        for k in others {
            if !recipients.contains(k) && !out.contains(k) {
                recipients.push(*k);
            }
        }
        let existing = match self.read(service).await {
            Ok(o) => o,
            Err(VaultStoreError::Vault(VaultError::NotARecipient)) => {
                return Err(VaultStoreError::NotOurs(service.to_string()))
            }
            Err(e) => return Err(e),
        };
        let (data_key, epoch, revision) = match &existing {
            None => (DataKey::generate(), 1, 1),
            Some(o) => {
                let dropped = o.recipients.iter().any(|k| !recipients.contains(k));
                if dropped {
                    (DataKey::generate(), o.epoch + 1, o.sign_in.revision + 1)
                } else {
                    (o.data_key.clone(), o.epoch, o.sign_in.revision + 1)
                }
            }
        };
        let mut s = sign_in.clone();
        s.service = service.to_string();
        s.revision = revision;
        if s.created_by.is_none() {
            s.created_by = Some(existing.as_ref().and_then(|o| o.sign_in.created_by).unwrap_or(me));
        }
        let plain =
            zeroize::Zeroizing::new(serde_json::to_vec(&s).map_err(|_| VaultError::Format("sign-in"))?);
        let env = spec::seal(&self.owner, path, epoch, &data_key, &plain, &recipients)?;
        let body = serde_json::to_vec(&env).map_err(|_| VaultError::Format("envelope"))?;
        self.put_locked(held, path, body).await?;
        Ok(revision)
    }

    /// Remove a service's sign-in from the vault (nothing to do when there is none).
    pub async fn delete(&self, service: &str) -> Result<()> {
        let path = Self::blob_path(service)?;
        match self.session.storage().delete(path.as_str()).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The services with a sign-in in the vault.
    pub async fn services(&self) -> Result<Vec<String>> {
        let storage = self.session.storage();
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut b = storage
                .list(spec::VAULT_DIR)?
                .limit(crate::homeserver::LIST_LIMIT)
                .shallow(true);
            if let Some(c) = &cursor {
                b = b.cursor(c);
            }
            let page = match b.send().await {
                Ok(p) => p,
                Err(e) if is_not_found(&e) => break,
                Err(e) => return Err(e.into()),
            };
            let n = page.len();
            cursor = page.last().map(|e| e.to_pubky_url());
            for e in page {
                let path = e.path.to_string();
                let name = path.rsplit('/').next().unwrap_or_default();
                if let Some(service) = name.strip_suffix(".json") {
                    if spec::is_service_id(service) {
                        out.push(service.to_string());
                    }
                }
            }
            if n < crate::homeserver::LIST_LIMIT as usize {
                break;
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// The keys the device list says were signed out: listed in `revoked` by key, or entries whose
/// grant id is there.
fn revoked_keys(ix: &Index) -> Vec<ClientKey> {
    let mut out: Vec<ClientKey> = ix.revoked.iter().filter_map(|r| r.client_key).collect();
    for e in &ix.clients {
        let jti = pubky::GrantClaims::decode(&e.proof.grant)
            .ok()
            .map(|c| c.jti.to_string());
        if ix.is_revoked(&e.client_key, jti.as_deref()) && !out.contains(&e.client_key) {
            out.push(e.client_key);
        }
    }
    out
}

/// How long a write lock is asked for (the homeserver caps it at 60 s).
const LOCK_FOR: Duration = Duration::from_secs(spec::LOCK_SECONDS);
/// Tries at a locked path before giving up (with a growing pause, about 2 s in all).
const LOCK_TRIES: u32 = 8;
/// `last_used_at` is written at most this often.
const TOUCH_EVERY_SECONDS: u64 = 24 * 3600;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn status(e: &pubky::Error) -> Option<reqwest::StatusCode> {
    match e {
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }) => Some(*status),
        _ => None,
    }
}

/// A write lock on one vault file, or none where the homeserver has no locks (405).
pub(crate) struct Locked {
    lock: Option<pubky::StorageLock>,
}

/// A trusted device-list entry: verified proof, owner's grant, not signed out.
#[derive(Debug, Clone)]
pub struct TrustedClient {
    pub client_key: ClientKey,
    pub name: String,
    pub last_used_at: u64,
    /// The grant's id (`jti`), what "Sign out this device" records.
    pub grant_id: String,
}

struct Checked {
    problem: Option<String>,
    grant_id: Option<String>,
    device: Option<String>,
    grant_expires_at: Option<u64>,
}

/// An entry of the device list, as Settings shows it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Device {
    pub client_key: String,
    /// The signed name (a trusted entry's name equals the one in its proof).
    pub name: String,
    /// The signed device kind: `node`, `desktop`, `android`, `ios`, `browser`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// When its Ring grant expires (Unix seconds): past it, the device needs approving again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_expires_at: Option<u64>,
    pub authorised_at: u64,
    pub last_used_at: u64,
    /// Its proof checks out (and it isn't signed out).
    pub trusted: bool,
    /// Why not, when it isn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// This node.
    pub this_node: bool,
}

impl Vault<'_> {
    /// Take the write lock on `path`: waits a little at a locked path, then [`VaultStoreError::Busy`].
    /// A homeserver without locks answers 405: then `Locked` holds none and writes go unlocked.
    pub(crate) async fn lock(&self, path: &str) -> Result<Locked> {
        let storage = self.session.storage();
        for attempt in 1..=LOCK_TRIES {
            match storage.lock(path, LOCK_FOR).await {
                Ok(l) => return Ok(Locked { lock: Some(l) }),
                Err(e) if status(&e) == Some(reqwest::StatusCode::METHOD_NOT_ALLOWED) => {
                    return Ok(Locked { lock: None })
                }
                Err(e) if status(&e) == Some(reqwest::StatusCode::LOCKED) => {
                    tokio::time::sleep(Duration::from_millis(50 * u64::from(attempt))).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(VaultStoreError::Busy(path.to_string()))
    }

    /// Write under a lock taken with [`Vault::lock`].
    pub(crate) async fn put_locked(&self, held: &Locked, path: &str, body: Vec<u8>) -> Result<()> {
        let storage = self.session.storage();
        match &held.lock {
            Some(l) => storage.put_locked(l, body).await?,
            None => storage.put(path, body).await?,
        };
        Ok(())
    }

    /// Release a lock (an expired one is fine: it lapses by itself).
    pub(crate) async fn unlock(&self, held: Locked) {
        if let Some(l) = held.lock {
            let _ = self.session.storage().unlock(&l).await;
        }
    }

    /// The device list (an empty one when there is none yet), with the signed-out clients this
    /// node remembers that name a listed entry added to `revoked`, so writing it back puts a
    /// dropped record back.
    pub async fn index(&self) -> Result<Index> {
        let mut ix = match self.read_bytes(spec::INDEX_PATH, spec::MAX_INDEX_BYTES).await? {
            Some(b) => Index::parse(&b)?,
            None => Index::default(),
        };
        let local = self.remember(&ix.revoked).await?;
        // Only the remembered records that name a listed entry change anyone's trust, so only
        // those go back into the list (and it stays small)
        let named: Vec<(ClientKey, Option<String>)> = ix
            .clients
            .iter()
            .map(|e| {
                (
                    e.client_key,
                    pubky::GrantClaims::decode(&e.proof.grant)
                        .ok()
                        .map(|c| c.jti.to_string()),
                )
            })
            .collect();
        let back: Vec<&Revoked> = local
            .iter()
            .filter(|r| named.iter().any(|(k, g)| r.names(k, g.as_deref())))
            .collect();
        ix.add_revoked(back);
        Ok(ix)
    }

    /// Why an entry isn't trusted (`None` = trusted), with its grant id when the proof verified.
    fn check(&self, ix: &Index, e: &ClientEntry) -> (Option<String>, Option<String>) {
        let c = self.checked(ix, e);
        (c.problem, c.grant_id)
    }

    /// An entry's proof checked: why it isn't trusted (`None` = trusted), its grant id, its signed
    /// device kind, and when its grant expires (readable even when it has).
    fn checked(&self, ix: &Index, e: &ClientEntry) -> Checked {
        let grant_expires_at = pubky::GrantClaims::decode(&e.proof.grant).ok().map(|c| c.exp);
        let creds = pubky::custom_pop::CustomPop {
            grant: e.proof.grant.clone(),
            pop: e.proof.pop.clone(),
        };
        let v = match pubky::custom_pop::verify_custom_grant_pop(
            &creds,
            pubky::custom_pop::DEFAULT_CUSTOM_POP_CLOCK_SKEW,
        ) {
            Ok(v) => v,
            Err(err) => {
                return Checked {
                    problem: Some(err.to_string()),
                    grant_id: None,
                    device: None,
                    grant_expires_at,
                }
            }
        };
        let caps: Vec<String> = v.grant_claims().caps.iter().map(|c| c.to_string()).collect();
        let jti = v.grant_claims().jti.to_string();
        let device = v
            .data()
            .get("device")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let problem = spec::client_entry_problem(e, &self.owner, &v.identity().z32(), &caps, v.data())
            .map(str::to_string)
            .or_else(|| {
                ix.is_revoked(&e.client_key, Some(&jti))
                    .then(|| "signed out".to_string())
            });
        Checked {
            problem,
            grant_id: Some(jti),
            device,
            grant_expires_at,
        }
    }

    /// The device list's entries that may open the vault: their proofs verify with the pubky SDK,
    /// the grant is the owner's and names the vault itself, and they weren't signed out.
    pub fn trusted(&self, ix: &Index) -> Vec<TrustedClient> {
        ix.clients
            .iter()
            .filter_map(|e| match self.check(ix, e) {
                (None, Some(jti)) => Some(TrustedClient {
                    client_key: e.client_key,
                    name: e.name.clone(),
                    last_used_at: e.last_used_at,
                    grant_id: jti,
                }),
                _ => None,
            })
            .collect()
    }

    /// The device list for Settings: every entry, trusted or not, and why not.
    pub async fn devices(&self) -> Result<Vec<Device>> {
        let ix = self.index().await?;
        let me = self.client_key();
        Ok(ix
            .clients
            .iter()
            .map(|e| {
                let c = self.checked(&ix, e);
                let problem = c.problem;
                Device {
                    client_key: e.client_key.to_hex(),
                    name: e.name.clone(),
                    device: c.device,
                    grant_expires_at: c.grant_expires_at,
                    authorised_at: e.authorised_at,
                    last_used_at: e.last_used_at,
                    trusted: problem.is_none(),
                    problem,
                    this_node: e.client_key == me,
                }
            })
            .collect())
    }

    /// Put this node in the device list as `name`, with a fresh proof of its key (made with the
    /// session's grant: Ring's approval binds it to the person). Refused when it was signed out.
    pub async fn register(&self, name: &str) -> Result<()> {
        let grant = self.session.as_grant().ok_or(VaultStoreError::NoGrant)?;
        // Other devices trust only a grant that names the vault itself
        let caps = crate::homeserver::session_capabilities(self.session);
        if !caps.iter().any(|c| spec::covers_vault(c)) {
            return Err(VaultStoreError::NeedsVaultGrant);
        }
        let me = self.client_key();
        let mut proof_data = ClientProofData::new(me, name);
        if let Some(kind) = self.device {
            proof_data = proof_data.device(kind);
        }
        let data = serde_json::to_value(proof_data).map_err(|_| VaultError::Format("proof data"))?;
        let pop = grant
            .create_custom_pop(data)
            .await
            .map_err(|_| VaultStoreError::NoGrant)?;
        // Other devices check the caps in the grant, so check those too
        let grant_caps = pubky::GrantClaims::decode(&pop.grant)
            .map(|c| c.caps.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .unwrap_or_default();
        if !grant_caps.iter().any(|c| spec::covers_vault(c)) {
            return Err(VaultStoreError::NeedsVaultGrant);
        }
        let held = self.lock(spec::INDEX_PATH).await?;
        let r = async {
            let mut ix = self.index().await?;
            let entry_jti = pubky::GrantClaims::decode(&pop.grant)
                .ok()
                .map(|c| c.jti.to_string());
            if ix.is_revoked(&me, entry_jti.as_deref()) {
                return Err(VaultStoreError::Revoked);
            }
            let t = now();
            let proof = Proof {
                grant: pop.grant.clone(),
                pop: pop.pop.clone(),
            };
            match ix.clients.iter_mut().find(|e| e.client_key == me) {
                Some(e) => {
                    e.name = name.to_string();
                    e.last_used_at = t;
                    e.proof = proof;
                }
                None => {
                    if ix.clients.len() >= spec::MAX_CLIENTS {
                        // Make room by dropping entries nobody trusts (junk, expired, signed out)
                        let keep: Vec<bool> = ix
                            .clients
                            .iter()
                            .map(|e| self.check(&ix, e).0.is_none())
                            .collect();
                        let mut k = keep.into_iter();
                        ix.clients.retain(|_| k.next().unwrap_or(true));
                    }
                    if ix.clients.len() >= spec::MAX_CLIENTS {
                        return Err(VaultError::TooLarge.into());
                    }
                    ix.clients.push(ClientEntry {
                        client_key: me,
                        name: name.to_string(),
                        authorised_at: t,
                        last_used_at: t,
                        proof,
                        extra: Default::default(),
                    })
                }
            }
            let body = index_body(&mut ix)?;
            self.put_locked(&held, spec::INDEX_PATH, body).await
        }
        .await;
        self.unlock(held).await;
        r
    }

    /// Register when this node isn't listed (or its proof no longer verifies), or once a day to
    /// keep `last_used_at` current.
    pub async fn touch(&self, name: &str) -> Result<()> {
        let ix = self.index().await?;
        let me = self.client_key();
        let fresh = ix.clients.iter().find(|e| e.client_key == me).is_some_and(|e| {
            now().saturating_sub(e.last_used_at) < TOUCH_EVERY_SECONDS && self.check(&ix, e).0.is_none()
        });
        if fresh {
            return Ok(());
        }
        self.register(name).await
    }

    /// Write a sign-in for every trusted device (and this node).
    ///
    /// Who is trusted is read under the blob's lock: a device signed out meanwhile is in the
    /// index before its sign-out takes any blob lock, so it is never wrapped to again.
    pub async fn write_shared(&self, service: &str, sign_in: &SignIn) -> Result<u64> {
        let path = Self::blob_path(service)?;
        let held = self.lock(&path).await?;
        let r = async {
            let ix = self.index().await?;
            let others: Vec<ClientKey> = self.trusted(&ix).into_iter().map(|t| t.client_key).collect();
            self.write_held(&held, service, &path, sign_in, &others).await
        }
        .await;
        self.unlock(held).await;
        r
    }

    /// Give every trusted device that has no key yet the blobs this node can open (a new device:
    /// same data key, same epoch). Returns how many blobs changed.
    pub async fn share_with_trusted(&self) -> Result<usize> {
        let ix = self.index().await?;
        let trusted: Vec<ClientKey> = self.trusted(&ix).into_iter().map(|t| t.client_key).collect();
        let mut changed = 0;
        for service in self.services().await? {
            match self.share_one(&service, &trusted).await {
                Ok(true) => changed += 1,
                Ok(false) => {}
                // One blob that can't be shared now doesn't stop the others; the next touch
                // tries it again
                Err(e) => tracing::warn!(service = %service, "vault: not shared with new devices yet: {e}"),
            }
        }
        Ok(changed)
    }

    /// Wrap one blob to the trusted devices that have no key in it. `hint` is the trusted list
    /// read before any lock, only to skip blobs with nothing to do; who gets a key is decided
    /// from the index read again under the blob's lock.
    async fn share_one(&self, service: &str, hint: &[ClientKey]) -> Result<bool> {
        let path = Self::blob_path(service)?;
        let Some(env) = self.envelope(service).await? else {
            return Ok(false);
        };
        let me = self.client_key();
        if !env.has_recipient(&me) || hint.iter().all(|k| env.has_recipient(k)) {
            return Ok(false);
        }
        let held = self.lock(&path).await?;
        let r = async {
            let ix = self.index().await?;
            let trusted: Vec<ClientKey> = self.trusted(&ix).into_iter().map(|t| t.client_key).collect();
            let Some(mut env) = self.envelope(service).await? else {
                return Ok(false);
            };
            if !env.has_recipient(&me) {
                return Ok(false);
            }
            let mut added = false;
            for k in &trusted {
                if !env.has_recipient(k) {
                    spec::add_recipient(&self.owner, &path, &mut env, self.secret, k)?;
                    added = true;
                }
            }
            if added {
                let body = serde_json::to_vec(&env).map_err(|_| VaultError::Format("envelope"))?;
                self.put_locked(&held, &path, body).await?;
            }
            Ok::<bool, VaultStoreError>(added)
        }
        .await;
        self.unlock(held).await;
        r
    }
}

/// How long a lease writer waits before reading its lease back (VAULT.md: 2 s).
const LEASE_SETTLE: Duration = Duration::from_secs(2);
/// How often a held lock is renewed while the service refreshes (a third of its life).
const RENEW_EVERY: Duration = Duration::from_secs(spec::LOCK_SECONDS / 3);
/// How long a client waits for another one's refresh before giving up.
const WAIT_FOR_OTHERS: Duration = Duration::from_secs(spec::LOCK_SECONDS + 5);
/// The longest a service is given to answer a refresh; then the lock is let go.
const CALL_FOR: Duration = Duration::from_secs(2 * spec::LOCK_SECONDS);
/// The longest one renewal of the lock or the lease may take.
const RENEW_FOR: Duration = Duration::from_secs(5);
/// Tries at writing refreshed tokens before handing them back unsaved.
const SAVE_TRIES: u32 = 6;

/// What [`Vault::refresh`] did.
#[derive(Debug)]
pub enum Refreshed {
    /// The tokens didn't need refreshing.
    Current(SignIn),
    /// Another client had refreshed them meanwhile: read under the lock, used as they are.
    ByAnother(SignIn),
    /// This client refreshed them and wrote them back.
    ByUs(SignIn),
}

impl Refreshed {
    pub fn sign_in(&self) -> &SignIn {
        match self {
            Self::Current(s) | Self::ByAnother(s) | Self::ByUs(s) => s,
        }
    }
}

/// How refreshed tokens were saved.
enum Saved {
    /// Written, as returned.
    Ours(SignIn),
    /// Another client's fresh copy was there: used instead.
    Theirs(SignIn),
}

/// The service refused the refresh (`String`: its words, never a token).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RefreshFailed(pub String);

impl Vault<'_> {
    /// Refresh a service's tokens without racing the person's other clients (VAULT.md, "Refreshing
    /// a token"). `refresh` gets the sign-in just read under the lock and returns it with new
    /// tokens and `expires_at`; it's called only when the tokens are due (or `force`, after the
    /// service refused them) and nobody refreshed them meanwhile. It never sees a token that wasn't
    /// read under the lock (or the lease).
    ///
    /// Once the service has answered, its new tokens are written back, retried if the write
    /// fails, or returned in [`VaultStoreError::Unsaved`]: they may be the only ones that still
    /// work. Dropping this future after the service answered loses them, so callers let it run.
    pub async fn refresh<F, Fut>(&self, service: &str, force: bool, refresh: F) -> Result<Refreshed>
    where
        F: FnOnce(SignIn) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<SignIn, RefreshFailed>>,
    {
        let path = Self::blob_path(service)?;
        let Some(first) = self.read(service).await? else {
            return Err(VaultError::Format("no sign-in to refresh").into());
        };
        if !force && !first.sign_in.needs_refresh(now()) {
            return Ok(Refreshed::Current(first.sign_in));
        }
        let seen = first.sign_in.revision;
        // Somebody else's fresh copy, read without the lock: good enough to use.
        let by_another = |o: &Opened| o.sign_in.revision > seen && !o.sign_in.needs_refresh(now());
        let deadline = tokio::time::Instant::now() + WAIT_FOR_OTHERS;
        let held = loop {
            let held = if self.lease_only {
                Locked { lock: None }
            } else {
                match self.lock(&path).await {
                    Ok(h) => h,
                    Err(VaultStoreError::Busy(_)) if tokio::time::Instant::now() < deadline => {
                        // Another client is refreshing: read again shortly
                        tokio::time::sleep(Duration::from_millis(1000 + jitter_ms(1000))).await;
                        if let Some(o) = self.read(service).await? {
                            if by_another(&o) {
                                return Ok(Refreshed::ByAnother(o.sign_in));
                            }
                        }
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            };
            if held.lock.is_some() {
                break held;
            }
            // No lock (405, or lease-only): the lease file
            match self.take_lease(service).await? {
                true => break held,
                false if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(1000 + jitter_ms(1000))).await;
                    if let Some(o) = self.read(service).await? {
                        if by_another(&o) {
                            return Ok(Refreshed::ByAnother(o.sign_in));
                        }
                    }
                }
                false => return Err(VaultStoreError::Busy(path.clone())),
            }
        };
        self.refresh_held(held, service, &path, seen, refresh).await
    }

    async fn refresh_held<F, Fut>(
        &self,
        mut held: Locked,
        service: &str,
        path: &str,
        seen: u64,
        refresh: F,
    ) -> Result<Refreshed>
    where
        F: FnOnce(SignIn) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<SignIn, RefreshFailed>>,
    {
        let leased = held.lock.is_none();
        let release = |held: Locked| async move {
            if leased {
                self.drop_lease(service).await;
            } else {
                self.unlock(held).await;
            }
        };
        // Read again under the lock: another client may have refreshed meanwhile
        let cur = match self.read(service).await {
            Ok(Some(o)) => o,
            Ok(None) => {
                release(held).await;
                return Err(VaultError::Format("the sign-in was removed meanwhile").into());
            }
            Err(e) => {
                release(held).await;
                return Err(e);
            }
        };
        // Even when the service refused our copy (force), a newer one another client wrote is the
        // one to try first
        if cur.sign_in.revision > seen && !cur.sign_in.needs_refresh(now()) {
            release(held).await;
            return Ok(Refreshed::ByAnother(cur.sign_in));
        }
        // The service call, with the lock (or the lease) renewed while it runs, and a limit
        let call = tokio::time::timeout(CALL_FOR, refresh(cur.sign_in.clone()));
        tokio::pin!(call);
        let mut renew = tokio::time::interval(RENEW_EVERY);
        renew.tick().await;
        let outcome = loop {
            tokio::select! {
                out = &mut call => break out,
                // Time-limited: the homeserver client has no timeouts of its own, and the
                // service's answer waits while this runs
                _ = renew.tick() => {
                    if tokio::time::timeout(RENEW_FOR, self.renew(&mut held, service)).await.is_err() {
                        tracing::warn!(service, "vault: renewing took too long; it is tried again in 10 s");
                    }
                }
            }
        };
        let mut fresh = match outcome {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                release(held).await;
                return Err(VaultStoreError::Refresh(e.0));
            }
            Err(_) => {
                release(held).await;
                return Err(VaultStoreError::Refresh(format!(
                    "the service didn't answer in {} s",
                    CALL_FOR.as_secs()
                )));
            }
        };
        fresh.last_refresh = Some(spec::LastRefresh {
            client: self.client_key(),
            at: now(),
        });
        fresh.service = service.to_string();
        // From here `fresh` may hold the only refresh token that still works: it is written, or
        // handed back
        let saved = self.save_refreshed(&mut held, service, path, &cur, &fresh).await;
        release(held).await;
        match saved {
            Ok(Saved::Ours(s)) => Ok(Refreshed::ByUs(s)),
            Ok(Saved::Theirs(s)) => Ok(Refreshed::ByAnother(s)),
            // Removed meanwhile (signed out everywhere), or rotated without this node (it was
            // signed out): not ours to keep
            Err(e @ (VaultStoreError::NotOurs(_) | VaultStoreError::Vault(VaultError::NotARecipient)))
            | Err(e @ VaultStoreError::Vault(VaultError::Format("the sign-in was removed meanwhile"))) => {
                // The service has used up the old refresh token, so the devices that still hold
                // this sign-in may need to sign in again
                tracing::warn!(
                    service,
                    "vault: refreshed tokens dropped, the sign-in changed hands meanwhile: {e}"
                );
                Err(e)
            }
            Err(e) => Err(VaultStoreError::Unsaved {
                service: service.to_string(),
                sign_in: Box::new(fresh),
                reason: e.to_string(),
            }),
        }
    }

    /// Write tokens the service just gave, trying again while the homeserver fails (and taking
    /// the lock again when it lapsed: ours are the live tokens, so waiting is worth it).
    async fn save_refreshed(
        &self,
        held: &mut Locked,
        service: &str,
        path: &str,
        cur: &Opened,
        fresh: &SignIn,
    ) -> Result<Saved> {
        let deadline = tokio::time::Instant::now() + WAIT_FOR_OTHERS;
        let mut attempt = 0;
        loop {
            attempt += 1;
            // Each try is held to what is left of the deadline: a write stalled on a dead
            // connection mustn't keep the tokens from being handed back
            let left = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .max(Duration::from_secs(1));
            let e = match tokio::time::timeout(left, self.save_once(held, service, path, cur, fresh)).await {
                Ok(Ok(s)) => return Ok(s),
                Ok(Err(e)) => e,
                Err(_) => VaultStoreError::Busy(path.to_string()),
            };
            if !e.transient() || attempt >= SAVE_TRIES || tokio::time::Instant::now() >= deadline {
                return Err(e);
            }
            tracing::warn!(
                service,
                attempt,
                "vault: the refreshed sign-in isn't saved yet: {e}"
            );
            let lapsed = matches!(
                e.http_status(),
                // The homeserver answers 412 to a write under a lock it no longer has
                Some(reqwest::StatusCode::PRECONDITION_FAILED)
            );
            if lapsed && held.lock.is_some() {
                let old = std::mem::replace(held, Locked { lock: None });
                self.unlock(old).await;
                *held = loop {
                    match self.lock(path).await {
                        Ok(h) => break h,
                        Err(VaultStoreError::Busy(_)) if tokio::time::Instant::now() < deadline => continue,
                        Err(e) => return Err(e),
                    }
                };
            } else {
                tokio::time::sleep(Duration::from_millis(250 * u64::from(attempt))).await;
            }
        }
    }

    /// One try at writing refreshed tokens. The blob is read again first:
    /// - another client's fresh copy (newer, other tokens, not due) stands, and ours are dropped;
    /// - a copy written meanwhile without new tokens (a new device, a sign-out rotation, a
    ///   setting) is kept, with our tokens in it and wrapped to its recipients.
    async fn save_once(
        &self,
        held: &Locked,
        service: &str,
        path: &str,
        cur: &Opened,
        fresh: &SignIn,
    ) -> Result<Saved> {
        let Some(now_o) = self.read(service).await? else {
            return Err(VaultError::Format("the sign-in was removed meanwhile").into());
        };
        let theirs = &now_o.sign_in;
        if theirs.revision > cur.sign_in.revision
            && theirs.tokens != cur.sign_in.tokens
            && !theirs.needs_refresh(now())
        {
            return Ok(Saved::Theirs(now_o.sign_in));
        }
        let mut s = if theirs.revision == cur.sign_in.revision {
            fresh.clone()
        } else {
            let mut s = theirs.clone();
            s.tokens = fresh.tokens.clone();
            s.expires_at = fresh.expires_at;
            s.scopes = fresh.scopes.clone();
            s.last_refresh = fresh.last_refresh;
            if fresh.user.is_some() {
                s.user = fresh.user.clone();
            }
            s
        };
        let me = self.client_key();
        let others: Vec<ClientKey> = now_o.recipients.iter().copied().filter(|k| *k != me).collect();
        s.revision = self.write_held(held, service, path, &s, &others).await?;
        Ok(Saved::Ours(s))
    }

    /// Renew what keeps other clients out while the service answers: the lock, or our lease.
    async fn renew(&self, held: &mut Locked, service: &str) {
        match held.lock.as_mut() {
            Some(l) => {
                if let Err(e) = self.session.storage().refresh_lock(l, LOCK_FOR).await {
                    // It still has up to 20 s; the write takes it again if it lapsed
                    tracing::warn!(service, "vault: the lock wasn't renewed: {e}");
                }
            }
            None => {
                if let Err(e) = self.renew_lease(service).await {
                    tracing::warn!(service, "vault: the lease wasn't renewed: {e}");
                }
            }
        }
    }

    /// Take the refresh lease (VAULT.md): `false` while another client's lease is live or ours
    /// didn't stick.
    async fn take_lease(&self, service: &str) -> Result<bool> {
        let path =
            spec::lease_path(service).ok_or_else(|| VaultStoreError::BadService(service.to_string()))?;
        let me = self.client_key();
        let read = || async {
            Ok::<Option<spec::Lease>, VaultStoreError>(
                self.read_bytes(&path, 1024)
                    .await?
                    .and_then(|b| serde_json::from_slice::<spec::Lease>(&b).ok()),
            )
        };
        if let Some(l) = read().await? {
            if l.client != me && l.until > now() {
                return Ok(false);
            }
        }
        let lease = spec::Lease {
            client: me,
            until: now() + spec::LOCK_SECONDS,
        };
        let body = serde_json::to_vec(&lease).map_err(|_| VaultError::Format("lease"))?;
        self.session.storage().put(path.as_str(), body).await?;
        tokio::time::sleep(LEASE_SETTLE).await;
        Ok(read().await?.is_some_and(|l| l.client == me))
    }

    /// Push our lease's `until` on while the service answers (only while it is still ours).
    async fn renew_lease(&self, service: &str) -> Result<()> {
        let path =
            spec::lease_path(service).ok_or_else(|| VaultStoreError::BadService(service.to_string()))?;
        let me = self.client_key();
        let ours = self
            .read_bytes(&path, 1024)
            .await?
            .and_then(|b| serde_json::from_slice::<spec::Lease>(&b).ok())
            .is_some_and(|l| l.client == me);
        if !ours {
            return Ok(());
        }
        let lease = spec::Lease {
            client: me,
            until: now() + spec::LOCK_SECONDS,
        };
        let body = serde_json::to_vec(&lease).map_err(|_| VaultError::Format("lease"))?;
        self.session.storage().put(path.as_str(), body).await?;
        Ok(())
    }

    /// Give the lease back: only ours, and only while it is live. Once it has run out another
    /// client may be taking it, and deleting could remove theirs.
    async fn drop_lease(&self, service: &str) {
        let Some(path) = spec::lease_path(service) else {
            return;
        };
        if let Ok(Some(b)) = self.read_bytes(&path, 1024).await {
            if serde_json::from_slice::<spec::Lease>(&b)
                .is_ok_and(|l| l.client == self.client_key() && l.until > now() + 1)
            {
                let _ = self.session.storage().delete(path.as_str()).await;
            }
        }
    }
}

/// What signing a device out did to one sign-in.
enum Rotation {
    Gone,
    NeverHad,
    NotReadableHere,
    Rotated,
}

/// What "Sign out this device" did.
#[derive(Debug, Default, serde::Serialize)]
pub struct SignOutReport {
    /// Sign-ins rotated past the device (new data key, epoch + 1).
    pub rotated: Vec<String>,
    /// Sign-ins this node can't read, so another device must rotate them.
    pub not_readable_here: Vec<String>,
    /// Sign-ins that couldn't be rotated now (busy, unreadable): signing out again retries them,
    /// and the next write of each leaves the device out anyway.
    pub failed: Vec<NotDone>,
}

/// A sign-in a sign-out couldn't handle now, and why.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NotDone {
    pub service: String,
    pub why: String,
}

/// What "Sign out everywhere" did.
#[derive(Debug, Default, serde::Serialize)]
pub struct EverywhereReport {
    /// Sign-ins deleted.
    pub removed: Vec<String>,
    /// Sign-ins still there (busy: another device was refreshing them): sign out again.
    pub left: Vec<NotDone>,
}

impl Vault<'_> {
    /// Sign a device out (VAULT.md, "Revocation"): it moves to `revoked` (by key and grant id),
    /// this node remembers that for good, and every sign-in this node can read gets a new data key
    /// wrapped only to the remaining trusted devices. One sign-in that can't be rotated now
    /// doesn't stop the others ([`SignOutReport::failed`]). `after` runs per rotated sign-in (the
    /// service-side part: a forced refresh, a token revocation, where the service allows it).
    ///
    /// The device's Pubky session isn't revoked here (that needs Ring): until it is, the device
    /// can still write vault files, but no client that remembers the sign-out gives it a key.
    /// This node can't sign itself out, by key or through its own grant.
    pub async fn sign_out_device<F, Fut>(&self, device: &ClientKey, after: F) -> Result<SignOutReport>
    where
        F: Fn(String, SignIn) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        if *device == self.client_key() {
            return Err(VaultError::Format("this node can't sign itself out of the vault").into());
        }
        let my_grant = match self.session.as_grant() {
            Some(g) => Some(g.grant_id().await.to_string()),
            None => None,
        };
        // 1. The device list: out of `clients`, into `revoked`
        let held = self.lock(spec::INDEX_PATH).await?;
        let r = async {
            let mut ix = self.index().await?;
            let jti = ix
                .clients
                .iter()
                .find(|e| e.client_key == *device)
                .and_then(|e| {
                    pubky::GrantClaims::decode(&e.proof.grant)
                        .ok()
                        .map(|c| c.jti.to_string())
                })
                // An old key of this node under the same grant (a restored node made a new
                // key): sign out the key only, or the node would sign itself out
                .filter(|j| my_grant.as_deref() != Some(j.as_str()));
            ix.clients.retain(|e| e.client_key != *device);
            let rec = spec::Revoked {
                client_key: Some(*device),
                grant_id: jti,
                at: now(),
            };
            // Remembered here first: the list itself can be written back without it
            self.remember(std::slice::from_ref(&rec))?;
            ix.add_revoked([&rec]);
            let body = serde_json::to_vec(&ix).map_err(|_| VaultError::Format("index"))?;
            self.put_locked(&held, spec::INDEX_PATH, body).await?;
            Ok::<Index, VaultStoreError>(ix)
        }
        .await;
        self.unlock(held).await;
        let ix = r?;
        // 2. Every sign-in: rotate past it, each on its own
        let trusted: Vec<ClientKey> = self.trusted(&ix).into_iter().map(|t| t.client_key).collect();
        let mut report = SignOutReport::default();
        for service in self.services().await? {
            match self.rotate_past(&service, device, &trusted).await {
                Ok(Rotation::Rotated) => {
                    if let Some(o) = self.read(&service).await.ok().flatten() {
                        after(service.clone(), o.sign_in).await;
                    }
                    report.rotated.push(service);
                }
                Ok(Rotation::NotReadableHere) => report.not_readable_here.push(service),
                Ok(Rotation::Gone | Rotation::NeverHad) => {}
                Err(e) => report.failed.push(NotDone {
                    service,
                    why: e.to_string(),
                }),
            }
        }
        Ok(report)
    }

    /// Rotate one sign-in past a signed-out device, under its lock.
    async fn rotate_past(
        &self,
        service: &str,
        device: &ClientKey,
        trusted: &[ClientKey],
    ) -> Result<Rotation> {
        let path = Self::blob_path(service)?;
        let held = self.lock(&path).await?;
        let r = async {
            let Some(env) = self.envelope(service).await? else {
                return Ok(Rotation::Gone);
            };
            if !env.has_recipient(device) {
                return Ok(Rotation::NeverHad);
            }
            let Ok((_, sign_in)) = spec::open_sign_in(&self.owner, &path, &env, self.secret) else {
                return Ok(Rotation::NotReadableHere);
            };
            let others: Vec<ClientKey> = trusted
                .iter()
                .copied()
                .filter(|k| k != device && *k != self.client_key())
                .collect();
            self.write_held(&held, service, &path, &sign_in, &others).await?;
            Ok::<Rotation, VaultStoreError>(Rotation::Rotated)
        }
        .await;
        self.unlock(held).await;
        r
    }

    /// "Sign out everywhere": delete every sign-in (and its lease), each under its lock; the
    /// device list stays, so the devices can take new sign-ins. `before` runs per sign-in this node
    /// can read, before it goes (the service-side revocation). A sign-in another device is
    /// refreshing right now is left and reported.
    pub async fn sign_out_everywhere<F, Fut>(&self, before: F) -> Result<EverywhereReport>
    where
        F: Fn(String, SignIn) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let mut report = EverywhereReport::default();
        for service in self.services().await? {
            match self.delete_one(&service, &before).await {
                Ok(()) => report.removed.push(service),
                Err(e) => report.left.push(NotDone {
                    service,
                    why: e.to_string(),
                }),
            }
        }
        Ok(report)
    }

    async fn delete_one<F, Fut>(&self, service: &str, before: &F) -> Result<()>
    where
        F: Fn(String, SignIn) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let path = Self::blob_path(service)?;
        let held = self.lock(&path).await?;
        if let Some(o) = self.read(service).await.ok().flatten() {
            before(service.to_string(), o.sign_in).await;
        }
        let storage = self.session.storage();
        let r = match &held.lock {
            Some(l) => storage.delete_locked(l).await.map(|_| ()),
            None => storage.delete(path.as_str()).await.map(|_| ()),
        };
        self.unlock(held).await;
        match r {
            Ok(()) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(lp) = spec::lease_path(service) {
            let _ = storage.delete(lp.as_str()).await;
        }
        Ok(())
    }
}

/// 0..max milliseconds, so clients that collide don't collide again (not secret: the clock's
/// nanoseconds are enough).
fn jitter_ms(max: u64) -> u64 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    n % max.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_key_is_the_noise_static_key_pubky_noise_derives() {
        for _ in 0..32 {
            let s = ClientSecret::generate();
            assert_eq!(s.public().0, pubky_noise::derive_static_public_key(&s.to_bytes()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn two_processes_making_the_key_at_once_get_the_same_key() {
        let dir = std::env::temp_dir().join(format!("pm-vault-race-{}", rand_suffix()));
        let path = dir.join("vault").join("client.key");
        let keys: Vec<[u8; 32]> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8)
                .map(|_| s.spawn(|| load_or_create_client_secret(&path).unwrap().public().0))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(keys.windows(2).all(|w| w[0] == w[1]));
        // Nothing left behind but the key
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn signed_out_clients_are_remembered_even_when_the_list_drops_them() {
        use pubky_music_specs::vault::{ClientKey, Revoked};
        let dir = std::env::temp_dir().join(format!("pm-vault-revoked-{}", rand_suffix()));
        let path = dir.join("vault-revoked.json");
        let me = ClientKey([9; 32]);
        let r = |n: u8, g: &str| Revoked {
            client_key: Some(ClientKey([n; 32])),
            grant_id: Some(g.into()),
            at: 1,
        };
        let remember = |list: &[Revoked]| remember_revoked(&path, list, &me, Some("mine")).unwrap();
        // Nothing yet
        assert!(remember(&[]).is_empty());
        // The list signs out D: remembered
        assert_eq!(remember(&[r(1, "d")]), [r(1, "d")]);
        // A later list without D (D rewrote it) still gives D back, and adds E
        assert_eq!(remember(&[r(2, "e")]), [r(1, "d"), r(2, "e")]);
        assert_eq!(remember(&[]).len(), 2);
        // The file is private
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Records naming nobody, or with a grant id that isn't one, are ignored; so are records
        // about this node (followed while the list says so, never remembered)
        let junk = [
            Revoked {
                client_key: None,
                grant_id: None,
                at: 1,
            },
            Revoked {
                client_key: None,
                grant_id: Some("x".repeat(200_000)),
                at: 1,
            },
            Revoked {
                client_key: None,
                grant_id: Some("not a grant id!".into()),
                at: 1,
            },
            Revoked {
                client_key: Some(me),
                grant_id: None,
                at: 1,
            },
            Revoked {
                client_key: None,
                grant_id: Some("mine".into()),
                at: 1,
            },
        ];
        assert_eq!(remember(&junk).len(), 2);
        // A flood fills only what is left for seen records; a sign-out this node makes is still
        // recorded, and kept
        let flood: Vec<Revoked> = (0..MAX_LOCAL_REVOKED + 10)
            .map(|i| r(3, &i.to_string()))
            .collect();
        assert_eq!(remember(&flood).len(), MAX_LOCAL_REVOKED);
        remember_made(&path, &r(4, "ours")).unwrap();
        let all = remember(&flood);
        assert_eq!(all.len(), MAX_LOCAL_REVOKED + 1);
        assert_eq!(all[0], r(4, "ours"));
        assert_eq!(&all[1..3], [r(1, "d"), r(2, "e")]);
        // The first form of the file (a plain list) still reads
        std::fs::write(&path, serde_json::to_vec(&[r(1, "d")]).unwrap()).unwrap();
        assert_eq!(remember(&[]), [r(1, "d")]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_device_list_written_back_stays_within_its_limit() {
        use pubky_music_specs::vault::{ClientKey, Revoked};
        // Junk records that name no listed entry: dropped to make it fit
        let revoked = (0..20_000u32)
            .map(|i| Revoked {
                client_key: Some(ClientKey([(i % 250) as u8; 32])),
                grant_id: Some(format!("g{i}")),
                at: 1,
            })
            .collect();
        let mut ix = Index {
            revoked,
            ..Index::default()
        };
        assert!(serde_json::to_vec(&ix).unwrap().len() > spec::MAX_INDEX_BYTES);
        let body = index_body(&mut ix).unwrap();
        assert!(body.len() <= spec::MAX_INDEX_BYTES);
        assert!(ix.revoked.is_empty());
        // A small list is written as it is
        ix.revoked.push(Revoked {
            client_key: Some(ClientKey([1; 32])),
            grant_id: None,
            at: 1,
        });
        index_body(&mut ix).unwrap();
        assert_eq!(ix.revoked.len(), 1);
    }

    fn rand_suffix() -> String {
        ClientSecret::generate().public().to_hex()[..12].to_string()
    }

    #[cfg(unix)]
    #[test]
    fn the_client_key_file() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("vault").join("client.key");
        let a = load_or_create_client_secret(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        // The same key on every start
        assert_eq!(load_or_create_client_secret(&p).unwrap().public(), a.public());
        // A file someone opened up is closed again
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_or_create_client_secret(&p).unwrap().public(), a.public());
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        // Something else in its place is refused, not replaced
        std::fs::write(&p, b"not a key").unwrap();
        assert!(load_or_create_client_secret(&p).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"not a key");
    }
}
