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
    SignIn, VaultError,
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
    #[error("this node was signed out of the vault; approve it again in Pubky Ring as a new device")]
    Revoked,
    #[error("this sign-in can't prove itself to other devices; sign the node in again with Pubky Ring")]
    NoGrant,
    #[error("another device is changing {0}; try again in a moment")]
    Busy(String),
    #[error("the service refused the refresh: {0}")]
    Refresh(String),
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
            Ok(secret)
        }
        // Another process made it first: use theirs
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(path),
        Err(e) => Err(e),
    }
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
}

impl<'a> Vault<'a> {
    pub fn new(session: &'a PubkySession, secret: &'a ClientSecret) -> Self {
        Self {
            session,
            owner: session.public_key().z32(),
            secret,
            lease_only: false,
            device: None,
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
        for k in others {
            if !recipients.contains(k) {
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

    /// The device list (an empty one when there is none yet).
    pub async fn index(&self) -> Result<Index> {
        match self.read_bytes(spec::INDEX_PATH, spec::MAX_INDEX_BYTES).await? {
            Some(b) => Ok(Index::parse(&b)?),
            None => Ok(Index::default()),
        }
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
    /// the grant is the owner's and covers the vault, and they weren't signed out.
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
            let body = serde_json::to_vec(&ix).map_err(|_| VaultError::Format("index"))?;
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
    pub async fn write_shared(&self, service: &str, sign_in: &SignIn) -> Result<u64> {
        let ix = self.index().await?;
        let others: Vec<ClientKey> = self.trusted(&ix).into_iter().map(|t| t.client_key).collect();
        self.write(service, sign_in, &others).await
    }

    /// Give every trusted device that has no key yet the blobs this node can open (a new device:
    /// same data key, same epoch). Returns how many blobs changed.
    pub async fn share_with_trusted(&self) -> Result<usize> {
        let ix = self.index().await?;
        let trusted: Vec<ClientKey> = self.trusted(&ix).into_iter().map(|t| t.client_key).collect();
        let mut changed = 0;
        for service in self.services().await? {
            let path = Self::blob_path(&service)?;
            let Some(env) = self.envelope(&service).await? else {
                continue;
            };
            if !env.has_recipient(&self.client_key()) || trusted.iter().all(|k| env.has_recipient(k)) {
                continue;
            }
            let held = self.lock(&path).await?;
            let r = async {
                // Read again under the lock
                let Some(mut env) = self.envelope(&service).await? else {
                    return Ok(false);
                };
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
            if r? {
                changed += 1;
            }
        }
        Ok(changed)
    }
}

/// How long a lease writer waits before reading its lease back (VAULT.md: 2 s).
const LEASE_SETTLE: Duration = Duration::from_secs(2);
/// How often a held lock is renewed while the service refreshes (a third of its life).
const RENEW_EVERY: Duration = Duration::from_secs(spec::LOCK_SECONDS / 3);
/// How long a client waits for another one's refresh before giving up.
const WAIT_FOR_OTHERS: Duration = Duration::from_secs(spec::LOCK_SECONDS + 5);

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
        let recipients = cur.recipients.clone();
        // The service call, with the lock renewed while it runs
        let call = refresh(cur.sign_in.clone());
        tokio::pin!(call);
        let mut renew = tokio::time::interval(RENEW_EVERY);
        renew.tick().await;
        let mut lost = false;
        let outcome = loop {
            tokio::select! {
                out = &mut call => break out,
                _ = renew.tick() => {
                    if let Some(l) = held.lock.as_mut() {
                        if self.session.storage().refresh_lock(l, LOCK_FOR).await.is_err() {
                            lost = true;
                        }
                    }
                }
            }
        };
        let mut fresh = match outcome {
            Ok(s) => s,
            Err(e) => {
                release(held).await;
                return Err(VaultStoreError::Refresh(e.0));
            }
        };
        fresh.last_refresh = Some(spec::LastRefresh {
            client: self.client_key(),
            at: now(),
        });
        if lost {
            // The lock lapsed while the service answered: take it again and write only if nobody
            // wrote meanwhile; otherwise theirs stands and ours is dropped.
            release(held).await;
            held = self.lock(path).await?;
            match self.read(service).await {
                Ok(Some(o)) if o.sign_in.revision == cur.sign_in.revision => {}
                Ok(Some(o)) => {
                    self.unlock(held).await;
                    return Ok(Refreshed::ByAnother(o.sign_in));
                }
                Ok(None) | Err(_) => {
                    self.unlock(held).await;
                    return Err(VaultStoreError::Busy(path.to_string()));
                }
            }
        }
        let others: Vec<ClientKey> = recipients
            .into_iter()
            .filter(|k| *k != self.client_key())
            .collect();
        let r = self.write_held(&held, service, path, &fresh, &others).await;
        release(held).await;
        let revision = r?;
        fresh.revision = revision;
        fresh.service = service.to_string();
        Ok(Refreshed::ByUs(fresh))
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

    /// Give the lease back (only ours).
    async fn drop_lease(&self, service: &str) {
        let Some(path) = spec::lease_path(service) else {
            return;
        };
        if let Ok(Some(b)) = self.read_bytes(&path, 1024).await {
            if serde_json::from_slice::<spec::Lease>(&b).is_ok_and(|l| l.client == self.client_key()) {
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
}

impl Vault<'_> {
    /// Sign a device out (VAULT.md, "Revocation"): it moves to `revoked` (by key and grant id, so
    /// it can't add itself back), and every sign-in this node can read gets a new data key wrapped
    /// only to the remaining trusted devices. `after` runs per rotated sign-in (the service-side
    /// part: a forced refresh, a token revocation, where the service allows it). This node can't
    /// sign itself out here: that would lock it out of its own vault.
    pub async fn sign_out_device<F, Fut>(&self, device: &ClientKey, after: F) -> Result<SignOutReport>
    where
        F: Fn(String, SignIn) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        if *device == self.client_key() {
            return Err(VaultError::Format("this node can't sign itself out of the vault").into());
        }
        // 1. The device list: out of `clients`, into `revoked`
        let held = self.lock(spec::INDEX_PATH).await?;
        let r = async {
            let mut ix = self.index().await?;
            let jti = ix.clients.iter().find(|e| e.client_key == *device).and_then(|e| {
                self.check(&ix, e).1.or_else(|| {
                    pubky::GrantClaims::decode(&e.proof.grant)
                        .ok()
                        .map(|c| c.jti.to_string())
                })
            });
            ix.clients.retain(|e| e.client_key != *device);
            if !ix.is_revoked(device, jti.as_deref()) {
                ix.revoked.push(spec::Revoked {
                    client_key: Some(*device),
                    grant_id: jti,
                    at: now(),
                });
            }
            let body = serde_json::to_vec(&ix).map_err(|_| VaultError::Format("index"))?;
            self.put_locked(&held, spec::INDEX_PATH, body).await?;
            Ok::<Index, VaultStoreError>(ix)
        }
        .await;
        self.unlock(held).await;
        let ix = r?;
        // 2. Every sign-in: rotate past it
        let trusted: Vec<ClientKey> = self.trusted(&ix).into_iter().map(|t| t.client_key).collect();
        let mut report = SignOutReport::default();
        for service in self.services().await? {
            let path = Self::blob_path(&service)?;
            let held = self.lock(&path).await?;
            let r = async {
                let Some(env) = self.envelope(&service).await? else {
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
                self.write_held(&held, &service, &path, &sign_in, &others).await?;
                Ok::<Rotation, VaultStoreError>(Rotation::Rotated)
            }
            .await;
            self.unlock(held).await;
            match r? {
                Rotation::Rotated => {
                    if let Some(o) = self.read(&service).await.ok().flatten() {
                        after(service.clone(), o.sign_in).await;
                    }
                    report.rotated.push(service);
                }
                Rotation::NotReadableHere => report.not_readable_here.push(service),
                Rotation::Gone | Rotation::NeverHad => {}
            }
        }
        Ok(report)
    }

    /// "Sign out everywhere": delete every sign-in (and its lease); the device list stays, so the
    /// devices can take new sign-ins. `before` runs per sign-in this node can read, before it goes
    /// (the service-side revocation). Returns the services removed.
    pub async fn sign_out_everywhere<F, Fut>(&self, before: F) -> Result<Vec<String>>
    where
        F: Fn(String, SignIn) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let mut gone = Vec::new();
        for service in self.services().await? {
            if let Some(o) = self.read(&service).await.ok().flatten() {
                before(service.clone(), o.sign_in).await;
            }
            self.delete(&service).await?;
            if let Some(lp) = spec::lease_path(&service) {
                let _ = self.session.storage().delete(lp.as_str()).await;
            }
            gone.push(service);
        }
        Ok(gone)
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
