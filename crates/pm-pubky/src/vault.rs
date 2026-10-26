//! The node's side of the sources vault (`crates/pubky-music-specs/VAULT.md`): its own client key,
//! and reading and writing sign-ins on the person's homeserver through their session.
//!
//! Everything is encrypted here before it leaves the node ([`pubky_music_specs::vault::seal`]);
//! the homeserver only ever sees envelopes. Which clients a blob is wrapped to is the caller's
//! choice: the trusted entries of the device list.

use crate::homeserver::{is_not_found, HomeserverError};
use pubky::PubkySession;
use pubky_music_specs::vault::{
    self as spec, ClientKey, ClientSecret, DataKey, Envelope, SignIn, VaultError,
};
use std::path::Path;

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
}

impl<'a> Vault<'a> {
    pub fn new(session: &'a PubkySession, secret: &'a ClientSecret) -> Self {
        Self {
            session,
            owner: session.public_key().z32(),
            secret,
        }
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
        let env = spec::seal(&self.owner, &path, epoch, &data_key, &plain, &recipients)?;
        let body = serde_json::to_vec(&env).map_err(|_| VaultError::Format("envelope"))?;
        self.session.storage().put(path.as_str(), body).await?;
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
