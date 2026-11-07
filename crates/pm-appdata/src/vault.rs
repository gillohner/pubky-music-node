//! The sources vault (`VAULT.md` in this crate): a person's streaming sign-ins on their own
//! homeserver, encrypted on the device to every client they have authorised, and, from v2,
//! signed by the device that wrote them ([`sign_envelope`], [`writer_signed`]).
//!
//! This module holds the paths, the device list ([`Index`]), the envelope ([`Envelope`]), the
//! sign-in it carries ([`SignIn`]) and the reference encryption ([`seal`], [`open`]). It does no I/O
//! and doesn't verify grant proofs: a client checks an entry's proof with the pubky SDK
//! (`verify_custom_grant_pop`) and then [`client_entry_problem`] with the verified parts.
//!
//! The primitives are pubky-noise's: X25519 (`curve25519-dalek`), HKDF-SHA-256 and
//! XChaCha20-Poly1305 (`chacha20poly1305`), plus Ed25519 (`ed25519-dalek`) for the writer's
//! signature. Nothing here implements a cipher.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use curve25519_dalek::montgomery::MontgomeryPoint;
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

/// The folder every vault file lives in.
pub const VAULT_DIR: &str = "/priv/pubky.music/vault/";
/// The device list.
pub const INDEX_PATH: &str = "/priv/pubky.music/vault/index.json";
/// The format version of the index, the envelope and the client proof.
pub const VERSION: u32 = 1;
/// `kind` in a client's custom proof data.
pub const CLIENT_KIND: &str = "pubky.music.vault-client";
/// Largest blob a reader accepts, in bytes.
pub const MAX_BLOB_BYTES: usize = 64 * 1024;
/// Largest index a reader accepts, in bytes.
pub const MAX_INDEX_BYTES: usize = 256 * 1024;
/// Most clients an index lists.
pub const MAX_CLIENTS: usize = 64;
/// How long a refresh lock or lease lives, in seconds.
pub const LOCK_SECONDS: u64 = 30;
/// How far ahead of `expires_at` a client refreshes, in seconds.
pub const REFRESH_AHEAD_SECONDS: u64 = 60;

const BLOB_DOMAIN: &[u8] = b"pubky.music/vault/v1/blob";
const WRAP_DOMAIN: &[u8] = b"pubky.music/vault/v1/wrap";

/// A service id: 1–32 of `a-z`, `0-9`, `-` (the source ids the node uses), except `index`, the
/// device list's name.
pub fn is_service_id(s: &str) -> bool {
    s != "index"
        && (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `/priv/pubky.music/vault/<service>.json`, or `None` for an invalid id.
pub fn blob_path(service: &str) -> Option<String> {
    is_service_id(service).then(|| format!("{VAULT_DIR}{service}.json"))
}

/// `/priv/pubky.music/vault/<service>.lease.json`: the refresh lease where locks are refused.
pub fn lease_path(service: &str) -> Option<String> {
    is_service_id(service).then(|| format!("{VAULT_DIR}{service}.lease.json"))
}

/// What went wrong with a vault file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// An unknown `version`.
    UnsupportedVersion(u32),
    /// Bad base64, a wrong length or a malformed member.
    Format(&'static str),
    /// This client has no `recipients` entry.
    NotARecipient,
    /// A key was low-order (the shared secret came out all zero).
    WeakKey,
    /// Authentication failed: the wrong key, or the file was changed.
    Decrypt,
    /// Over the size limits.
    TooLarge,
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "vault version {v} is not supported"),
            Self::Format(what) => write!(f, "vault file is malformed: {what}"),
            Self::NotARecipient => f.write_str("this client can't open this vault file"),
            Self::WeakKey => f.write_str("a vault key is weak (low order)"),
            Self::Decrypt => f.write_str("the vault file doesn't decrypt (wrong key or changed)"),
            Self::TooLarge => f.write_str("the vault file is too large"),
        }
    }
}

impl std::error::Error for VaultError {}

// ---- keys ----

/// A client's X25519 public key, lowercase hex in JSON.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientKey(pub [u8; 32]);

fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str, what: &'static str) -> Result<[u8; 32], VaultError> {
    if s.len() != 64 || !s.is_ascii() {
        return Err(VaultError::Format(what));
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| VaultError::Format(what))?;
    }
    Ok(out)
}

impl ClientKey {
    /// Lowercase hex.
    pub fn to_hex(&self) -> String {
        hex32(&self.0)
    }

    /// From 64 hex digits (either case).
    pub fn from_hex(s: &str) -> Result<Self, VaultError> {
        unhex32(s, "a client key is 64 hex digits").map(Self)
    }
}

impl std::fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClientKey({})", self.to_hex())
    }
}

impl Serialize for ClientKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ClientKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// A client's X25519 static secret. Kept on the device; zeroized on drop; never printed.
pub struct ClientSecret(Zeroizing<[u8; 32]>);

impl ClientSecret {
    /// A new random secret.
    pub fn generate() -> Self {
        Self(Zeroizing::new(random()))
    }

    /// From stored bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The bytes to store (keep them secret).
    pub fn to_bytes(&self) -> [u8; 32] {
        *self.0
    }

    /// The public key: `X25519(secret, base)`, as pubky-noise's `derive_static_public_key`.
    pub fn public(&self) -> ClientKey {
        ClientKey(MontgomeryPoint::mul_base_clamped(*self.0).to_bytes())
    }
}

impl std::fmt::Debug for ClientSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientSecret(<redacted>)")
    }
}

// ---- the signing key (v2) ----

/// The `info` of the signing key's derivation ([`SigningKey::derive`]).
const SIGN_DOMAIN: &[u8] = b"pubky.music/vault/v2/sign";
/// The domain of a writer's signature over an envelope ([`signed_data`]).
const WRITER_DOMAIN: &[u8] = b"pubky.music/vault/v2/blob";
/// The `kind` of a signer proof's data ([`SignerProofData`]).
pub const SIGNER_KIND: &str = "pubky.music.vault-signer";

/// A client's Ed25519 public key, the one that signs the blobs it writes; lowercase hex in JSON.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SignKey(pub [u8; 32]);

impl SignKey {
    /// Lowercase hex.
    pub fn to_hex(&self) -> String {
        hex32(&self.0)
    }

    /// From 64 hex digits (either case).
    pub fn from_hex(s: &str) -> Result<Self, VaultError> {
        unhex32(s, "a signing key is 64 hex digits").map(Self)
    }

    /// Whether `sig` (64 bytes) is this key's signature of `data` (Ed25519, strict verification).
    pub fn verify(&self, data: &[u8], sig: &[u8]) -> bool {
        let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(&self.0) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(sig) else {
            return false;
        };
        key.verify_strict(data, &ed25519_dalek::Signature::from_bytes(&sig))
            .is_ok()
    }
}

impl std::fmt::Debug for SignKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignKey({})", self.to_hex())
    }
}

impl Serialize for SignKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for SignKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// A client's Ed25519 signing key, derived from its X25519 secret: `seed = HKDF-SHA-256(salt
/// none, ikm = secret, info = "pubky.music/vault/v2/sign")`, so there is no second secret to
/// keep, and a backup of the client secret carries it.
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    /// The signing key of the client holding `secret`.
    pub fn derive(secret: &ClientSecret) -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        Hkdf::<Sha256>::new(None, &secret.0[..])
            .expand(SIGN_DOMAIN, seed.as_mut())
            .expect("32 bytes is a valid HKDF-SHA-256 output length");
        Self(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    /// Its public key.
    pub fn public(&self) -> SignKey {
        SignKey(self.0.verifying_key().to_bytes())
    }

    /// The Ed25519 signature of `data`.
    pub fn sign(&self, data: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        self.0.sign(data).to_bytes()
    }
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SigningKey({})", self.public().to_hex())
    }
}

/// The data a client signs in its signer proof (its second custom proof): the signing key
/// bound to its client key by the same grant as its client proof.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerProofData {
    /// [`SIGNER_KIND`].
    pub kind: String,
    /// [`VERSION`].
    pub v: u32,
    /// The client's X25519 public key.
    pub client_key: ClientKey,
    /// The client's Ed25519 public key.
    pub sign_key: SignKey,
}

impl SignerProofData {
    /// The signer proof data for a client.
    pub fn new(client_key: ClientKey, sign_key: SignKey) -> Self {
        Self {
            kind: SIGNER_KIND.into(),
            v: VERSION,
            client_key,
            sign_key,
        }
    }
}

/// A blob's data key. Zeroized on drop; never printed.
#[derive(Clone)]
pub struct DataKey(Zeroizing<[u8; 32]>);

impl DataKey {
    /// A new random key (at a blob's first write and at every rotation).
    pub fn generate() -> Self {
        Self(Zeroizing::new(random()))
    }

    /// From known bytes (test vectors).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }
}

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(<redacted>)")
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random source works");
    b
}

// ---- the device list ----

/// `index.json`: the clients that may open the vault, and the ones that never may again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Index {
    /// [`VERSION`].
    pub version: u32,
    /// The device list.
    #[serde(default)]
    pub clients: Vec<ClientEntry>,
    /// Signed-out devices: never wrapped to again, even if they add themselves back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revoked: Vec<Revoked>,
    /// Members this reader doesn't know, kept on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            version: VERSION,
            clients: Vec::new(),
            revoked: Vec::new(),
            extra: Map::new(),
        }
    }
}

impl Index {
    /// Parse, refusing other versions and oversized files.
    pub fn parse(bytes: &[u8]) -> Result<Self, VaultError> {
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(VaultError::TooLarge);
        }
        let ix: Self = serde_json::from_slice(bytes).map_err(|_| VaultError::Format("index.json"))?;
        if ix.version != VERSION {
            return Err(VaultError::UnsupportedVersion(ix.version));
        }
        // More than MAX_CLIENTS entries is read anyway (the byte cap bounds it): entries that
        // don't verify could otherwise make the whole list unreadable
        Ok(ix)
    }

    /// Add `more` to the signed-out list, skipping ones already in it. Returns how many were new.
    pub fn add_revoked<'r>(&mut self, more: impl IntoIterator<Item = &'r Revoked>) -> usize {
        let mut n = 0;
        for r in more {
            if !self.revoked.iter().any(|x| x.same_client(r)) {
                self.revoked.push(r.clone());
                n += 1;
            }
        }
        n
    }

    /// Whether a client (by key or by its grant id) was signed out.
    pub fn is_revoked(&self, key: &ClientKey, grant_id: Option<&str>) -> bool {
        self.revoked.iter().any(|r| {
            r.client_key.as_ref() == Some(key) || (grant_id.is_some() && r.grant_id.as_deref() == grant_id)
        })
    }
}

/// One authorised client in the device list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientEntry {
    /// Its X25519 public key.
    pub client_key: ClientKey,
    /// What the person calls it ("Living room node").
    pub name: String,
    /// Unix seconds.
    pub authorised_at: u64,
    /// Unix seconds; written at most once a day.
    pub last_used_at: u64,
    /// The grant and the custom proof that bind `client_key` to the identity.
    pub proof: Proof,
    /// The grant and the custom proof that bind the client's signing key to `client_key`
    /// ([`SignerProofData`]); absent for a client from before v2, which signs nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<Proof>,
    /// Members this reader doesn't know, kept on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The pubky SDK's `CustomPop` as stored: the root-signed grant JWS and the client-signed proof JWS.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    /// Compact grant JWS (`typ: pubky-grant`).
    pub grant: String,
    /// Compact proof JWS (`typ: pubky-custom-pop-v1`).
    pub pop: String,
}

impl std::fmt::Debug for Proof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Proof { .. }")
    }
}

/// A signed-out client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revoked {
    /// Its key, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<ClientKey>,
    /// Its grant id (`jti`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// Unix seconds.
    pub at: u64,
}

impl Revoked {
    /// It names a client: a key, or a grant id of the SDK's form (base64url, at most 64
    /// characters). Records that name nobody are ignored.
    pub fn is_well_formed(&self) -> bool {
        let grant_ok = |g: &str| {
            !g.is_empty()
                && g.len() <= 64
                && g.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        match (&self.client_key, &self.grant_id) {
            (_, Some(g)) if !grant_ok(g) => false,
            (Some(_), _) | (None, Some(_)) => true,
            (None, None) => false,
        }
    }

    /// It names `key`, or the grant `grant_id`.
    pub fn names(&self, key: &ClientKey, grant_id: Option<&str>) -> bool {
        self.client_key.as_ref() == Some(key) || (grant_id.is_some() && self.grant_id.as_deref() == grant_id)
    }

    /// The same signed-out client (same key and grant id), whenever it was recorded.
    pub fn same_client(&self, other: &Revoked) -> bool {
        self.client_key == other.client_key && self.grant_id == other.grant_id
    }
}

/// The data a client signs in its custom proof.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientProofData {
    /// [`CLIENT_KIND`].
    pub kind: String,
    /// [`VERSION`].
    pub v: u32,
    /// The client's X25519 public key.
    pub client_key: ClientKey,
    /// The same name as in the index (readers show this one: it is signed).
    pub name: String,
    /// What it is, for the icon: `node`, `desktop`, `android`, `ios`, `browser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl ClientProofData {
    /// The proof data for a client.
    pub fn new(client_key: ClientKey, name: impl Into<String>) -> Self {
        Self {
            kind: CLIENT_KIND.into(),
            v: VERSION,
            client_key,
            name: name.into(),
            device: None,
        }
    }

    /// With the kind of device (`node`, `desktop`, `android`, `ios`, `browser`).
    pub fn device(mut self, device: impl Into<String>) -> Self {
        self.device = Some(device.into());
        self
    }
}

/// Why an index entry isn't trusted, given its proof already verified by the pubky SDK
/// (`verify_custom_grant_pop`): `owner` is the vault owner's z32 key, `issuer` the grant's
/// `iss`, `caps` the grant's capabilities as strings (`/priv/pubky.music/vault/:rw`), and `data` the
/// proof's data. `None` = trusted.
pub fn client_entry_problem(
    entry: &ClientEntry,
    owner: &str,
    issuer: &str,
    caps: &[String],
    data: &Value,
) -> Option<&'static str> {
    if issuer != owner {
        return Some("the grant is someone else's");
    }
    if !caps.iter().any(|c| covers_vault(c)) {
        return Some("its grant doesn't name the vault (an app with access to a wider folder isn't trusted with the sign-ins)");
    }
    let Ok(d) = serde_json::from_value::<ClientProofData>(data.clone()) else {
        return Some("the proof's data isn't a vault client's");
    };
    if d.kind != CLIENT_KIND || d.v != VERSION {
        return Some("the proof's data isn't a vault client's");
    }
    if d.client_key != entry.client_key {
        return Some("the proof is for another key");
    }
    if d.name != entry.name {
        return Some("its name was changed outside its proof");
    }
    None
}

/// The signing key an entry's signer proof gives it, given that proof already verified by the
/// pubky SDK (`verify_custom_grant_pop`), with the same arguments as [`client_entry_problem`];
/// else why not.
pub fn signer_of_entry(
    entry: &ClientEntry,
    owner: &str,
    issuer: &str,
    caps: &[String],
    data: &Value,
) -> Result<SignKey, &'static str> {
    if issuer != owner {
        return Err("the signer proof's grant is someone else's");
    }
    if !caps.iter().any(|c| covers_vault(c)) {
        return Err("the signer proof's grant doesn't name the vault");
    }
    let Ok(d) = serde_json::from_value::<SignerProofData>(data.clone()) else {
        return Err("the signer proof's data isn't a vault signer's");
    };
    if d.kind != SIGNER_KIND || d.v != VERSION {
        return Err("the signer proof's data isn't a vault signer's");
    }
    if d.client_key != entry.client_key {
        return Err("the signer proof is for another key");
    }
    if ed25519_dalek::VerifyingKey::from_bytes(&d.sign_key.0).is_err() {
        return Err("the signer proof's signing key isn't one");
    }
    Ok(d.sign_key)
}

/// Whether a capability (`<scope>:<actions>`) names [`VAULT_DIR`] itself with `r` and `w`. An
/// ancestor (`/:rw`, `/priv/pubky.music/:rw`) doesn't count: the person approved that app for a
/// wider folder in Ring, not for their streaming sign-ins.
pub fn covers_vault(cap: &str) -> bool {
    let Some((scope, actions)) = cap.rsplit_once(':') else {
        return false;
    };
    scope == VAULT_DIR && actions.contains('r') && actions.contains('w')
}

/// The capability a vault client asks Ring for.
pub const VAULT_CAPABILITY: &str = "/priv/pubky.music/vault/:rw";

/// `<service>.lease.json`: who refreshes, until when (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// The client holding the lease.
    pub client: ClientKey,
    /// Unix seconds.
    pub until: u64,
}

// ---- the sign-in a blob carries ----

/// A blob's plaintext: one service sign-in.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SignIn {
    /// The service id.
    pub service: String,
    /// The account name the service shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The Sign in chooser's way it was made with: `app`, `device`, `own`, `import`, `paste`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub way: Option<String>,
    /// For `import`: the app it came from (`tidal-dl-ng`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The client id used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The person's own app secret, where it signs every request (Qobuz, Last.fm). Never a
    /// built-in app's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The service's tokens (`access_token`, `refresh_token`, `token_type`: `bearer`,
    /// `session_key` or `api_key`, …).
    #[serde(default)]
    pub tokens: Map<String, Value>,
    /// The account as the service names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ServiceUser>,
    /// Granted scopes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// When the access token expires, Unix seconds; absent or `null`: it doesn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The client that made the sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<ClientKey>,
    /// The last refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<LastRefresh>,
    /// Goes up by one at every write.
    #[serde(default)]
    pub revision: u64,
    /// What follows the person across clients (the quality choice, …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub settings: Map<String, Value>,
    /// Members this reader doesn't know, kept on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl std::fmt::Debug for SignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignIn")
            .field("service", &self.service)
            .field("way", &self.way)
            .field("revision", &self.revision)
            .field("tokens", &"<redacted>")
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish_non_exhaustive()
    }
}

impl SignIn {
    /// Whether the access token should be refreshed now (60 s ahead of expiry).
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|e| now + REFRESH_AHEAD_SECONDS >= e)
    }
}

/// The account as the service names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceUser {
    /// The service's id for the account.
    pub id: String,
    /// Its display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its country (TIDAL's API calls take it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Members this reader doesn't know, kept on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Who refreshed last, and when (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRefresh {
    /// The client.
    pub client: ClientKey,
    /// Unix seconds.
    pub at: u64,
}

// ---- the envelope ----

/// `<service>.json`: one sign-in, encrypted to every recipient.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// [`VERSION`].
    pub version: u32,
    /// Goes up at every data-key rotation (a device signed out).
    pub epoch: u64,
    /// 24 bytes, unpadded base64url; fresh at every write.
    pub nonce: String,
    /// XChaCha20-Poly1305 ciphertext with its tag, unpadded base64url.
    pub ciphertext: String,
    /// The data key, wrapped to each client.
    pub recipients: Vec<Recipient>,
    /// Who wrote it, and their signature over it ([`signed_data`]); absent on a blob from before
    /// v2. Readers that don't know the member keep it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<Writer>,
}

/// The client that wrote an envelope and its signature over it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Writer {
    /// The writer's X25519 public key (its index entry holds its signing key).
    pub client_key: ClientKey,
    /// Ed25519 signature of [`signed_data`] with the writer's signing key, unpadded base64url.
    pub sig: String,
}

/// The data key wrapped to one client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recipient {
    /// The client's X25519 public key.
    pub client_key: ClientKey,
    /// The ephemeral X25519 public key, unpadded base64url.
    pub epk: String,
    /// The wrapped data key (32 bytes and a 16-byte tag), unpadded base64url.
    pub wrapped_key: String,
}

impl Envelope {
    /// Parse, refusing other versions and oversized files.
    pub fn parse(bytes: &[u8]) -> Result<Self, VaultError> {
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(VaultError::TooLarge);
        }
        let env: Self = serde_json::from_slice(bytes).map_err(|_| VaultError::Format("envelope"))?;
        if env.version != VERSION {
            return Err(VaultError::UnsupportedVersion(env.version));
        }
        if env.recipients.len() > MAX_CLIENTS {
            return Err(VaultError::TooLarge);
        }
        Ok(env)
    }

    /// Whether `key` can open it.
    pub fn has_recipient(&self, key: &ClientKey) -> bool {
        self.recipients.iter().any(|r| r.client_key == *key)
    }
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64<const N: usize>(s: &str, what: &'static str) -> Result<[u8; N], VaultError> {
    let v = URL_SAFE_NO_PAD.decode(s).map_err(|_| VaultError::Format(what))?;
    v.try_into().map_err(|_| VaultError::Format(what))
}

/// `aad` of the content: `"pubky.music/vault/v1/blob" 0 owner 0 path 0 epoch(8, BE)`.
fn blob_aad(owner: &str, path: &str, epoch: u64) -> Vec<u8> {
    let mut a = Vec::with_capacity(BLOB_DOMAIN.len() + owner.len() + path.len() + 11);
    a.extend_from_slice(BLOB_DOMAIN);
    a.push(0);
    a.extend_from_slice(owner.as_bytes());
    a.push(0);
    a.extend_from_slice(path.as_bytes());
    a.push(0);
    a.extend_from_slice(&epoch.to_be_bytes());
    a
}

/// The key that wraps the data key for one recipient.
fn wrap_key(
    shared: &[u8; 32],
    epk: &[u8; 32],
    client_key: &ClientKey,
    owner: &str,
    path: &str,
    epoch: u64,
) -> Zeroizing<[u8; 32]> {
    let mut info = Vec::with_capacity(WRAP_DOMAIN.len() + 64 + owner.len() + path.len() + 11);
    info.extend_from_slice(WRAP_DOMAIN);
    info.push(0);
    info.extend_from_slice(epk);
    info.extend_from_slice(&client_key.0);
    info.extend_from_slice(owner.as_bytes());
    info.push(0);
    info.extend_from_slice(path.as_bytes());
    info.push(0);
    info.extend_from_slice(&epoch.to_be_bytes());
    let mut okm = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(None, shared)
        .expand(&info, okm.as_mut())
        .expect("32 bytes is a valid HKDF-SHA-256 output length");
    okm
}

fn x25519(secret: &[u8; 32], public: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    let shared = Zeroizing::new(MontgomeryPoint(*public).mul_clamped(*secret).to_bytes());
    if shared.iter().all(|b| *b == 0) {
        return Err(VaultError::WeakKey);
    }
    Ok(shared)
}

/// Wrap `data_key` to `client_key` with the ephemeral secret `eph` (fresh per wrap; fixed only in
/// test vectors).
pub fn wrap_with(
    owner: &str,
    path: &str,
    epoch: u64,
    data_key: &DataKey,
    client_key: &ClientKey,
    eph: &[u8; 32],
) -> Result<Recipient, VaultError> {
    let epk = MontgomeryPoint::mul_base_clamped(*eph).to_bytes();
    let shared = x25519(eph, &client_key.0)?;
    let k = wrap_key(&shared, &epk, client_key, owner, path, epoch);
    let wrapped = XChaCha20Poly1305::new(&(*k).into())
        .encrypt(&XNonce::default(), &data_key.0[..])
        .map_err(|_| VaultError::Decrypt)?;
    Ok(Recipient {
        client_key: *client_key,
        epk: b64(&epk),
        wrapped_key: b64(&wrapped),
    })
}

/// Wrap `data_key` to `client_key` with a fresh ephemeral key.
pub fn wrap(
    owner: &str,
    path: &str,
    epoch: u64,
    data_key: &DataKey,
    client_key: &ClientKey,
) -> Result<Recipient, VaultError> {
    let mut eph: [u8; 32] = random();
    let r = wrap_with(owner, path, epoch, data_key, client_key, &eph);
    eph.zeroize();
    r
}

/// The data key, unwrapped with this client's secret.
pub fn unwrap_data_key(
    owner: &str,
    path: &str,
    env: &Envelope,
    secret: &ClientSecret,
) -> Result<DataKey, VaultError> {
    if env.version != VERSION {
        return Err(VaultError::UnsupportedVersion(env.version));
    }
    let me = secret.public();
    let r = env
        .recipients
        .iter()
        .find(|r| r.client_key == me)
        .ok_or(VaultError::NotARecipient)?;
    let epk: [u8; 32] = unb64(&r.epk, "epk")?;
    let shared = x25519(&secret.0, &epk)?;
    let k = wrap_key(&shared, &epk, &me, owner, path, env.epoch);
    let wrapped = URL_SAFE_NO_PAD
        .decode(&r.wrapped_key)
        .map_err(|_| VaultError::Format("wrapped_key"))?;
    let plain = Zeroizing::new(
        XChaCha20Poly1305::new(&(*k).into())
            .decrypt(&XNonce::default(), wrapped.as_slice())
            .map_err(|_| VaultError::Decrypt)?,
    );
    let key: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| VaultError::Format("wrapped_key"))?;
    Ok(DataKey::from_bytes(key))
}

/// Encrypt `plaintext` for `path` with `data_key` and `nonce`, wrapping the key to each recipient
/// with its own ephemeral secret. Deterministic: the test vectors use it; [`seal`] picks the
/// randomness.
pub fn seal_with(
    owner: &str,
    path: &str,
    epoch: u64,
    data_key: &DataKey,
    nonce: &[u8; 24],
    plaintext: &[u8],
    recipients: &[(ClientKey, [u8; 32])],
) -> Result<Envelope, VaultError> {
    if recipients.len() > MAX_CLIENTS {
        return Err(VaultError::TooLarge);
    }
    let aad = blob_aad(owner, path, epoch);
    let ciphertext = XChaCha20Poly1305::new(&(*data_key.0).into())
        .encrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| VaultError::Decrypt)?;
    let recipients = recipients
        .iter()
        .map(|(k, eph)| wrap_with(owner, path, epoch, data_key, k, eph))
        .collect::<Result<Vec<_>, _>>()?;
    let env = Envelope {
        version: VERSION,
        epoch,
        nonce: b64(nonce),
        ciphertext: b64(&ciphertext),
        recipients,
        writer: None,
    };
    if serde_json::to_vec(&env).map_or(0, |v| v.len()) > MAX_BLOB_BYTES {
        return Err(VaultError::TooLarge);
    }
    Ok(env)
}

/// Encrypt `plaintext` for `path` with `data_key`, to every recipient, with a fresh nonce and fresh
/// ephemeral keys.
pub fn seal(
    owner: &str,
    path: &str,
    epoch: u64,
    data_key: &DataKey,
    plaintext: &[u8],
    recipients: &[ClientKey],
) -> Result<Envelope, VaultError> {
    let nonce: [u8; 24] = random();
    let ephs: Vec<(ClientKey, [u8; 32])> = recipients.iter().map(|k| (*k, random())).collect();
    let env = seal_with(owner, path, epoch, data_key, &nonce, plaintext, &ephs);
    for (_, mut e) in ephs {
        e.zeroize();
    }
    env
}

/// Decrypt with this client's secret: the data key (to write the blob again) and the plaintext.
pub fn open(
    owner: &str,
    path: &str,
    env: &Envelope,
    secret: &ClientSecret,
) -> Result<(DataKey, Zeroizing<Vec<u8>>), VaultError> {
    let key = unwrap_data_key(owner, path, env, secret)?;
    let nonce: [u8; 24] = unb64(&env.nonce, "nonce")?;
    let ct = URL_SAFE_NO_PAD
        .decode(&env.ciphertext)
        .map_err(|_| VaultError::Format("ciphertext"))?;
    let aad = blob_aad(owner, path, env.epoch);
    let plain = XChaCha20Poly1305::new(&(*key.0).into())
        .decrypt(&XNonce::from(nonce), Payload { msg: &ct, aad: &aad })
        .map_err(|_| VaultError::Decrypt)?;
    Ok((key, Zeroizing::new(plain)))
}

/// Add a recipient to an envelope this client can open (a new device), keeping the epoch.
pub fn add_recipient(
    owner: &str,
    path: &str,
    env: &mut Envelope,
    secret: &ClientSecret,
    new_client: &ClientKey,
) -> Result<(), VaultError> {
    if env.has_recipient(new_client) {
        return Ok(());
    }
    if env.recipients.len() >= MAX_CLIENTS {
        return Err(VaultError::TooLarge);
    }
    let key = unwrap_data_key(owner, path, env, secret)?;
    env.recipients
        .push(wrap(owner, path, env.epoch, &key, new_client)?);
    Ok(())
}

/// What a writer signs over an envelope: `"pubky.music/vault/v2/blob" 0 owner 0 path 0 epoch(8,
/// BE)` ‖ `SHA-256(each recipient's client_key ‖ epk ‖ wrapped_key, raw, in order)` ‖
/// `SHA-256(nonce ‖ ciphertext, raw)`. The revision and every other plaintext member are
/// covered through the ciphertext; a change of recipients, content, epoch or path breaks it.
pub fn signed_data(owner: &str, path: &str, env: &Envelope) -> Result<Vec<u8>, VaultError> {
    use sha2::Digest;
    let mut r = Sha256::new();
    for x in &env.recipients {
        r.update(x.client_key.0);
        r.update(unb64::<32>(&x.epk, "epk")?);
        r.update(unb64::<48>(&x.wrapped_key, "wrapped_key")?);
    }
    let mut c = Sha256::new();
    c.update(unb64::<24>(&env.nonce, "nonce")?);
    c.update(
        URL_SAFE_NO_PAD
            .decode(&env.ciphertext)
            .map_err(|_| VaultError::Format("ciphertext"))?,
    );
    let mut d = Vec::with_capacity(WRITER_DOMAIN.len() + owner.len() + path.len() + 11 + 64);
    d.extend_from_slice(WRITER_DOMAIN);
    d.push(0);
    d.extend_from_slice(owner.as_bytes());
    d.push(0);
    d.extend_from_slice(path.as_bytes());
    d.push(0);
    d.extend_from_slice(&env.epoch.to_be_bytes());
    d.extend_from_slice(&r.finalize());
    d.extend_from_slice(&c.finalize());
    Ok(d)
}

/// Sign `env` as `client_key` with its signing key (every write, and every rewrite: a device
/// added, a rotation).
pub fn sign_envelope(
    owner: &str,
    path: &str,
    env: &mut Envelope,
    client_key: ClientKey,
    key: &SigningKey,
) -> Result<(), VaultError> {
    let data = signed_data(owner, path, env)?;
    env.writer = Some(Writer {
        client_key,
        sig: b64(&key.sign(&data)),
    });
    Ok(())
}

/// Whether `env` carries a signature by `sign_key` (the signing key of its `writer`, as that
/// client's index entry proves it). An unsigned envelope is not.
pub fn writer_signed(owner: &str, path: &str, env: &Envelope, sign_key: &SignKey) -> bool {
    let Some(w) = &env.writer else {
        return false;
    };
    let Ok(sig) = unb64::<64>(&w.sig, "sig") else {
        return false;
    };
    let Ok(data) = signed_data(owner, path, env) else {
        return false;
    };
    sign_key.verify(&data, &sig)
}

/// A blob's sign-in, decrypted and parsed.
pub fn open_sign_in(
    owner: &str,
    path: &str,
    env: &Envelope,
    secret: &ClientSecret,
) -> Result<(DataKey, SignIn), VaultError> {
    let (key, plain) = open(owner, path, env, secret)?;
    let s = serde_json::from_slice(&plain).map_err(|_| VaultError::Format("sign-in"))?;
    Ok((key, s))
}
