//! Backup and restore (steward next-4, item 1): one file with everything needed to rebuild the
//! node, never the audio.
//!
//! The file (`pubky-music-<date>.pmbackup`) is a zip:
//! - `manifest.json`: what it is, when, from which node, and counts (tracks, matches, playlists…);
//! - `library.db`: a snapshot of the library database (matches and corrections, playlists,
//!   favourites, tags, play history, queues, settings) with every sign-in taken out;
//! - `secrets.bin`, only when the owner chose a passphrase: the sign-ins (TIDAL, Spotify, Jamendo,
//!   ListenBrainz, your Subsonic and Jellyfin servers), the Pubky session, the remote-access key
//!   and the node's config file, encrypted with AES-256-GCM under a key derived from the
//!   passphrase with Argon2id. Without a passphrase they are left out, and the restore says
//!   which services to sign in to again.
//!
//! A restore swaps the database in while the node runs, puts the sign-ins back, reconnects Pubky,
//! and rescans: unchanged files are skipped, so every match stays. Music that now lives
//! elsewhere is pointed to its new folder with `POST /v1/library/folders/move`.
//!
//! Scheduled backups go to a folder the owner picks, keeping the last N.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use pm_library::{BackupCounts, Library};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a backup file says it is.
pub const FORMAT: &str = "pubky-music-backup";
pub const VERSION: u32 = 1;

/// Settings that hold sign-ins: only ever in the encrypted part.
pub const SECRET_SETTINGS: &[(&str, &str)] = &[
    ("tidal.user", "TIDAL"),
    ("tidal.unofficial", "TIDAL full tracks"),
    ("tidal.unofficial_client", "the TIDAL device client"),
    ("youtube.api_key", "your YouTube Data API key"),
    ("acoustid.api_key", "your AcoustID key"),
    ("qobuz.unofficial", "Qobuz"),
    ("qobuz.app", "the Qobuz app"),
    ("bandcamp.identity", "your Bandcamp sign-in"),
    ("soundcloud.unofficial", "SoundCloud"),
    ("soundcloud.client", "the SoundCloud client and sign-in token"),
    ("spotify.user", "Spotify"),
    ("spotify.soloist_key", "your Spotify Soloist API key"),
    ("jamendo.user", "Jamendo"),
    ("listenbrainz.account", "ListenBrainz"),
    ("lastfm.app", "the Last.fm app"),
    ("lastfm.session", "Last.fm"),
    ("opensubsonic.server", "your Subsonic server"),
    ("jellyfin.server", "Jellyfin"),
    ("plex.server", "Plex"),
    ("audiobookshelf.server", "Audiobookshelf"),
    ("webdav.shares", "your WebDAV shares"),
    ("podcasts.gpodder", "your gPodder account"),
    ("drives.accounts", "your Google Drive and OneDrive sign-ins"),
];
/// Settings that belong to this node and are never carried over (its stream key, its id, and
/// the backup schedule with its passphrase).
const NODE_OWN: &[&str] = &["stream.secret", "node.id", SCHEDULE_KEY];
const SCHEDULE_KEY: &str = "backup.schedule";
/// The passphrase arrives in this header, never in a URL (URLs end up in logs).
pub const PASSPHRASE_HEADER: &str = "x-backup-passphrase";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub created_at: i64,
    pub node_name: String,
    pub node_version: String,
    pub counts: BackupCounts,
    /// The library folders, as paths on the node that made it.
    pub folders: Vec<String>,
    /// How the secrets are locked; `None` when they were left out.
    pub secrets: Option<Sealed>,
    /// What was left out for lack of a passphrase (shown on restore).
    #[serde(default)]
    pub left_out: Vec<String>,
}

/// Argon2id parameters and the AES-GCM nonce for `secrets.bin`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sealed {
    pub kdf: String,
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
    pub salt: String,
    pub nonce: String,
    pub cipher: String,
}

/// The decrypted secrets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Secrets {
    pub settings: BTreeMap<String, String>,
    /// File name in the data folder → contents (base64).
    pub files: BTreeMap<String, String>,
    /// The node's config file, as it was.
    pub config: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("This is not a Pubky Music backup ({0})")]
    NotABackup(String),
    #[error("This backup was made by a newer version of Pubky Music (format {0}); update this node first")]
    TooNew(u32),
    #[error("That passphrase does not open this backup")]
    WrongPassphrase,
    #[error("This backup holds sign-ins locked with a passphrase: enter it, or restore without them")]
    NeedsPassphrase,
    #[error("library: {0}")]
    Library(#[from] pm_library::LibraryError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
}

impl From<BackupError> for ApiError {
    fn from(e: BackupError) -> Self {
        let status = match e {
            BackupError::NotABackup(_) | BackupError::TooNew(_) => StatusCode::UNPROCESSABLE_ENTITY,
            BackupError::WrongPassphrase | BackupError::NeedsPassphrase => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(status, e.to_string())
    }
}

// ---- encryption ----------------------------------------------------------------------------

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn derive(passphrase: &str, salt: &[u8], m_kib: u32, t: u32, p: u32) -> [u8; 32] {
    let params = argon2::Params::new(m_kib, t, p, Some(32)).expect("valid argon2 params");
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    a.hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .expect("argon2 with valid params");
    key
}

pub fn seal(secrets: &Secrets, passphrase: &str) -> (Sealed, Vec<u8>) {
    use rand::RngCore;
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce);
    let (m_kib, t, p) = (19 * 1024, 2, 1);
    let key = derive(passphrase, &salt, m_kib, t, p);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32-byte key");
    let plain = serde_json::to_vec(secrets).expect("secrets serialize");
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
        .expect("encryption");
    (
        Sealed {
            kdf: "argon2id".into(),
            m_kib,
            t,
            p,
            salt: b64().encode(salt),
            nonce: b64().encode(nonce),
            cipher: "aes-256-gcm".into(),
        },
        sealed,
    )
}

pub fn open_sealed(s: &Sealed, data: &[u8], passphrase: &str) -> Result<Secrets, BackupError> {
    if s.kdf != "argon2id" || s.cipher != "aes-256-gcm" || s.m_kib > 1024 * 1024 || s.t > 64 || s.p > 16 {
        return Err(BackupError::NotABackup("unknown encryption".into()));
    }
    let salt = b64()
        .decode(&s.salt)
        .map_err(|_| BackupError::NotABackup("salt".into()))?;
    let nonce = b64()
        .decode(&s.nonce)
        .map_err(|_| BackupError::NotABackup("nonce".into()))?;
    if nonce.len() != 12 {
        return Err(BackupError::NotABackup("nonce".into()));
    }
    let key = derive(passphrase, &salt, s.m_kib, s.t, s.p);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32-byte key");
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), data)
        .map_err(|_| BackupError::WrongPassphrase)?;
    serde_json::from_slice(&plain).map_err(|_| BackupError::NotABackup("secrets".into()))
}

// ---- making a backup -------------------------------------------------------------------------

fn data_dir(s: &NodeState) -> PathBuf {
    s.db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn session_file(s: &NodeState) -> PathBuf {
    s.session_secret_path
        .clone()
        .unwrap_or_else(|| data_dir(s).join("pubky-session.secret"))
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pm-{name}-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ))
}

fn remove_db(path: &Path) {
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}

/// The backup file's bytes. `passphrase`: lock the sign-ins in; `None` leaves them out.
pub fn make(s: &NodeState, passphrase: Option<&str>) -> Result<(Vec<u8>, Manifest), BackupError> {
    let snap = scratch("backup.db");
    let (counts, folders, mut secrets) = {
        let lib = s.library.lock().expect("library poisoned");
        lib.backup_to(&snap)?;
        let mut secrets = Secrets::default();
        for (key, _) in SECRET_SETTINGS {
            if let Some(v) = lib.get_setting(key)? {
                secrets.settings.insert((*key).to_string(), v);
            }
        }
        let folders = lib
            .list_folders()?
            .into_iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        (lib.backup_counts()?, folders, secrets)
    };
    // The snapshot without sign-ins and without what belongs to this node only
    let db = (|| -> Result<Vec<u8>, BackupError> {
        {
            let copy = Library::open(&snap)?;
            for (key, _) in SECRET_SETTINGS {
                copy.delete_setting(key)?;
            }
            for key in NODE_OWN {
                copy.delete_setting(key)?;
            }
            // The vault cache's state belongs to this machine (its records name its vault key,
            // and sign-ins moved aside after "Sign out everywhere" are still tokens): never in a
            // backup
            for (key, _) in copy.settings_with_prefix("vault.")? {
                copy.delete_setting(&key)?;
            }
            copy.vacuum_into_place()?;
        }
        Ok(std::fs::read(&snap)?)
    })();
    remove_db(&snap);
    let db = db?;

    let session = session_file(s);
    if let Ok(v) = std::fs::read(&session) {
        secrets
            .files
            .insert("pubky-session.secret".into(), b64().encode(v));
    }
    if let Ok(v) = std::fs::read(data_dir(s).join("remote.key")) {
        secrets.files.insert("remote.key".into(), b64().encode(v));
    }
    // The API token the node made for itself (no `[api] token`), which controllers keep
    if let Ok(v) = std::fs::read(data_dir(s).join("api-token")) {
        secrets.files.insert("api-token".into(), b64().encode(v));
    }
    if let Some(cfg) = s.config_file() {
        secrets.config = std::fs::read_to_string(cfg).ok();
    }

    let mut left_out = Vec::new();
    let sealed = match passphrase.filter(|p| !p.is_empty()) {
        Some(p) => Some(seal(&secrets, p)),
        None => {
            for (key, name) in SECRET_SETTINGS {
                if secrets.settings.contains_key(*key) {
                    left_out.push(format!("the {name} sign-in"));
                }
            }
            if secrets.files.contains_key("pubky-session.secret") {
                left_out.push("the Pubky sign-in".into());
            }
            if secrets.files.contains_key("remote.key") {
                left_out.push("the remote-access address".into());
            }
            if secrets.files.contains_key("api-token") {
                left_out.push("the node's API token".into());
            }
            if secrets.config.is_some() {
                left_out.push("the node's config file".into());
            }
            None
        }
    };
    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        node_name: s.name.clone(),
        node_version: s.version.clone(),
        counts,
        folders,
        secrets: sealed.as_ref().map(|(m, _)| m.clone()),
        left_out,
    };

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let deflate =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("manifest.json", deflate)?;
    zip.write_all(&serde_json::to_vec_pretty(&manifest).expect("manifest serializes"))?;
    zip.start_file("library.db", deflate)?;
    zip.write_all(&db)?;
    if let Some((_, data)) = &sealed {
        zip.start_file("secrets.bin", stored)?;
        zip.write_all(data)?;
    }
    let bytes = zip.finish()?.into_inner();
    Ok((bytes, manifest))
}

/// A backup's parts.
pub struct Parsed {
    pub manifest: Manifest,
    pub database: Vec<u8>,
    pub secrets: Option<Vec<u8>>,
}

pub fn parse(bytes: &[u8]) -> Result<Parsed, BackupError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| BackupError::NotABackup("not a zip file".into()))?;
    let mut read = |name: &str| -> Result<Option<Vec<u8>>, BackupError> {
        match zip.by_name(name) {
            Ok(mut f) => {
                let mut v = Vec::new();
                f.read_to_end(&mut v)?;
                Ok(Some(v))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    let manifest: Manifest = serde_json::from_slice(
        &read("manifest.json")?.ok_or_else(|| BackupError::NotABackup("no manifest".into()))?,
    )
    .map_err(|_| BackupError::NotABackup("unreadable manifest".into()))?;
    if manifest.format != FORMAT {
        return Err(BackupError::NotABackup(manifest.format));
    }
    if manifest.version > VERSION {
        return Err(BackupError::TooNew(manifest.version));
    }
    let database = read("library.db")?.ok_or_else(|| BackupError::NotABackup("no library".into()))?;
    let secrets = read("secrets.bin")?;
    Ok(Parsed {
        manifest,
        database,
        secrets,
    })
}

// ---- restoring -------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreReport {
    pub from_node: String,
    pub made_at: i64,
    /// What the backup said it held, and what this node holds now.
    pub expected: BackupCounts,
    pub restored: BackupCounts,
    /// Sign-ins put back (service names).
    pub signed_in: Vec<String>,
    /// Sign-ins that were not in the file (made without a passphrase, or skipped).
    pub sign_in_again: Vec<String>,
    /// Library folders that do not exist on this node: point them to where the music is now.
    pub missing_folders: Vec<String>,
    pub rescan_started: bool,
    pub notes: Vec<String>,
}

/// Restore `bytes` into the running node. `passphrase` opens the sign-ins; with `skip_secrets`
/// a passphrase-locked backup is restored without them.
pub async fn restore(
    s: &Arc<NodeState>,
    bytes: Vec<u8>,
    passphrase: Option<String>,
    skip_secrets: bool,
) -> Result<RestoreReport, ApiError> {
    let parsed = parse(&bytes)?;
    let m = &parsed.manifest;
    let secrets = match (
        &m.secrets,
        &parsed.secrets,
        passphrase.as_deref().filter(|p| !p.is_empty()),
    ) {
        (Some(sealed), Some(data), Some(p)) => {
            let (sealed, data, p) = (sealed.clone(), data.clone(), p.to_string());
            Some(
                tokio::task::spawn_blocking(move || open_sealed(&sealed, &data, &p))
                    .await
                    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??,
            )
        }
        (Some(_), _, None) if !skip_secrets => return Err(BackupError::NeedsPassphrase.into()),
        _ => None,
    };
    if !s.begin_scan() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a scan is running; restore when it is done",
        ));
    }
    let result = restore_locked(s, &parsed, secrets).await;
    s.end_scan();
    let mut report = result?;
    // Covers first (the extracted ones were not carried over), then a rescan that skips every
    // unchanged file and so keeps the matches.
    let present: Vec<PathBuf> = s.all_folders().into_iter().filter(|p| p.is_dir()).collect();
    let state = s.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(lib) = Library::open(&state.db_path) {
            let n = pm_library::Scanner::new(&lib)
                .with_art_dir(&state.art_dir)
                .fill_missing_art()
                .unwrap_or(0);
            tracing::info!(covers = n, "covers read again after a restore");
        }
    })
    .await
    .ok();
    report.rescan_started = !present.is_empty() && matches!(s.start_scan(present), Ok(true));
    s.notify_library();
    Ok(report)
}

async fn restore_locked(
    s: &Arc<NodeState>,
    parsed: &Parsed,
    secrets: Option<Secrets>,
) -> Result<RestoreReport, ApiError> {
    let m = &parsed.manifest;
    let staged = scratch("restore.db");
    std::fs::write(&staged, &parsed.database)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Open (and migrate) the copy first: a broken file never replaces a working library
    let checked = Library::open(&staged).and_then(|l| {
        l.vacuum_into_place()?;
        Ok(())
    });
    if let Err(e) = checked {
        remove_db(&staged);
        return Err(BackupError::NotABackup(format!("the library in it does not open: {e}")).into());
    }
    let mut report = RestoreReport {
        from_node: m.node_name.clone(),
        made_at: m.created_at,
        expected: m.counts.clone(),
        ..Default::default()
    };
    let done = (|| -> Result<(), BackupError> {
        let lib = s.library.lock().expect("library poisoned");
        let keep: Vec<(&str, Option<String>)> = NODE_OWN
            .iter()
            .map(|k| Ok((*k, lib.get_setting(k)?)))
            .collect::<Result<_, BackupError>>()?;
        let signins_here: Vec<(&str, Option<String>)> = SECRET_SETTINGS
            .iter()
            .map(|(k, _)| Ok((*k, lib.get_setting(k)?)))
            .collect::<Result<_, BackupError>>()?;
        lib.restore_from(&staged)?;
        for (k, v) in keep {
            match v {
                Some(v) => lib.set_setting(k, &v)?,
                None => {
                    lib.delete_setting(k)?;
                }
            }
        }
        for (key, name) in SECRET_SETTINGS {
            match secrets.as_ref().and_then(|x| x.settings.get(*key)) {
                Some(v) => {
                    lib.set_setting(key, v)?;
                    report.signed_in.push((*name).to_string());
                }
                None => {
                    // A sign-in made on this node before the restore stays
                    if let Some((_, Some(v))) = signins_here.iter().find(|(k, _)| k == key) {
                        lib.set_setting(key, v)?;
                    }
                }
            }
        }
        lib.drop_missing_art()?;
        report.restored = lib.backup_counts()?;
        report.missing_folders = lib
            .list_folders()?
            .into_iter()
            .filter(|(p, _)| !p.is_dir())
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        Ok(())
    })();
    remove_db(&staged);
    done?;
    s.reload_sources_off();
    if secrets.is_none() {
        report.sign_in_again = m
            .left_out
            .iter()
            .filter(|w| w.contains("sign-in"))
            .cloned()
            .collect();
        if m.secrets.is_some() {
            report
                .notes
                .push("Restored without the sign-ins locked in this backup.".into());
        }
    }
    let dir = data_dir(s);
    if let Some(sec) = &secrets {
        for (name, value) in &sec.files {
            let Ok(bytes) = b64().decode(value) else {
                continue;
            };
            let path = match name.as_str() {
                "pubky-session.secret" => session_file(s),
                "remote.key" => dir.join("remote.key"),
                "api-token" => dir.join("api-token"),
                _ => continue,
            };
            let written = crate::imports::write_private(&path, &bytes).is_ok();
            if written && name == "remote.key" {
                report
                    .notes
                    .push("Remote access keeps its old address after the node restarts.".into());
            }
            if written && name == "api-token" {
                report.notes.push(
                    "The node's own API token comes back after the node restarts (unless its config sets one).".into(),
                );
            }
        }
        if let Some(cfg) = &sec.config {
            let path = dir
                .canonicalize()
                .unwrap_or(dir.clone())
                .join("node.restored.toml");
            if crate::imports::write_private(&path, cfg.as_bytes()).is_ok() {
                report.notes.push(format!(
                    "The old node's config file is in {}: the node uses its service keys where its own config leaves them empty, from the next start.",
                    path.display()
                ));
            }
        }
        // Pubky: reconnect with the restored session
        if let Some(secret) = sec
            .files
            .get("pubky-session.secret")
            .and_then(|v| b64().decode(v).ok())
            .and_then(|v| String::from_utf8(v).ok())
        {
            match crate::sync::PubkySync::connect_with(
                s.homeserver()
                    .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?,
                &secret,
            )
            .await
            {
                Ok(sync) => {
                    let sync = Arc::new(sync);
                    report
                        .signed_in
                        .push(format!("Pubky ({})", short_key(sync.z32())));
                    s.set_sync(Some(sync.clone()));
                    let st = s.clone();
                    tokio::spawn(async move {
                        if let Ok(r) = sync.sync(&st.library).await {
                            sync.carry_on(&st, &r);
                        }
                    });
                }
                Err(e) => report.notes.push(format!(
                    "The Pubky sign-in could not be restored ({e}): sign in again."
                )),
            }
        }
    }
    // The contact for Discover, from the restored settings
    let contact = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting("node.contact")
        .ok()
        .flatten();
    if let Some(c) = contact.filter(|c| !c.trim().is_empty()) {
        let _ = s.set_contact(&c);
    }
    Ok(report)
}

/// A Pubky key as people read it: no `pubky` prefix, shortened in the middle.
pub fn short_key(k: &str) -> String {
    let k = k.trim_start_matches("pubky");
    if k.chars().count() > 20 {
        let head: String = k.chars().take(6).collect();
        let tail: String = k
            .chars()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}…{tail}")
    } else {
        k.to_string()
    }
}

// ---- schedule --------------------------------------------------------------------------------

/// Where and how often scheduled backups go. The passphrase is kept on this node only (never
/// in a backup); without one, scheduled backups leave the sign-ins out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    /// A folder on the node; empty = off.
    #[serde(default)]
    pub folder: String,
    #[serde(default = "default_every")]
    pub every_hours: u32,
    #[serde(default = "default_keep")]
    pub keep: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub last_at: Option<i64>,
    #[serde(default)]
    pub last_file: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

fn default_every() -> u32 {
    24
}
fn default_keep() -> u32 {
    7
}

/// What the page shows: the schedule without its passphrase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleView {
    pub folder: String,
    pub every_hours: u32,
    pub keep: u32,
    pub with_signins: bool,
    pub last_at: Option<i64>,
    pub last_file: Option<String>,
    pub last_error: Option<String>,
    /// Backups now in the folder, newest first.
    pub files: Vec<String>,
}

fn load_schedule(s: &NodeState) -> Schedule {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(SCHEDULE_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or(Schedule {
            every_hours: default_every(),
            keep: default_keep(),
            ..Default::default()
        })
}

fn save_schedule(s: &NodeState, sch: &Schedule) -> Result<(), ApiError> {
    s.library.lock().expect("library poisoned").set_setting(
        SCHEDULE_KEY,
        &serde_json::to_string(sch).expect("schedule serializes"),
    )?;
    Ok(())
}

const FILE_PREFIX: &str = "pubky-music-";
const FILE_EXT: &str = ".pmbackup";

/// Backups in `folder` this node wrote, newest first.
pub fn backups_in(folder: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(folder)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(FILE_PREFIX) && n.ends_with(FILE_EXT))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

/// Keep the newest `keep` backups in `folder`; returns the names removed.
pub fn prune(folder: &Path, keep: usize) -> Vec<String> {
    let mut removed = Vec::new();
    for name in backups_in(folder).into_iter().skip(keep.max(1)) {
        if std::fs::remove_file(folder.join(&name)).is_ok() {
            removed.push(name);
        }
    }
    removed
}

fn file_name(now: time::OffsetDateTime) -> String {
    format!(
        "{FILE_PREFIX}{:04}-{:02}-{:02}-{:02}{:02}{:02}{FILE_EXT}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Write one scheduled backup now (and prune).
pub async fn run_scheduled(s: &Arc<NodeState>) -> Result<String, String> {
    let mut sch = load_schedule(s);
    if sch.folder.trim().is_empty() {
        return Err("no backup folder set".into());
    }
    let folder = PathBuf::from(sch.folder.trim());
    let st = s.clone();
    let pass = sch.passphrase.clone();
    let made = tokio::task::spawn_blocking(move || -> Result<String, String> {
        std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        let (bytes, _) = make(&st, pass.as_deref()).map_err(|e| e.to_string())?;
        let name = file_name(time::OffsetDateTime::now_utc());
        let tmp = folder.join(format!(".{name}.part"));
        std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, folder.join(&name)).map_err(|e| e.to_string())?;
        Ok(name)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    sch.last_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
    match &made {
        Ok(name) => {
            sch.last_file = Some(name.clone());
            sch.last_error = None;
            let removed = prune(Path::new(sch.folder.trim()), sch.keep as usize);
            tracing::info!(file = %name, removed = removed.len(), "scheduled backup written");
        }
        Err(e) => {
            sch.last_error = Some(e.clone());
            tracing::warn!("scheduled backup failed: {e}");
        }
    }
    let _ = save_schedule(s, &sch);
    made
}

/// Check every few minutes whether a scheduled backup is due.
pub fn start_schedule(s: &Arc<NodeState>) {
    let s = s.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            let sch = load_schedule(&s);
            if sch.folder.trim().is_empty() {
                continue;
            }
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let due = sch
                .last_at
                .is_none_or(|t| now - t >= i64::from(sch.every_hours.max(1)) * 3600);
            if due {
                let _ = run_scheduled(&s).await;
            }
        }
    });
}

// ---- HTTP ------------------------------------------------------------------------------------

fn passphrase_of(h: &HeaderMap) -> Option<String> {
    h.get(PASSPHRASE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|p| !p.is_empty())
}

fn download(bytes: Vec<u8>) -> Response {
    let name = file_name(time::OffsetDateTime::now_utc());
    (
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /v1/backup`: a backup without the sign-ins (a plain link).
pub(crate) async fn get_backup(State(s): State<Arc<NodeState>>) -> Result<Response, ApiError> {
    let (bytes, _) = tokio::task::spawn_blocking(move || make(&s, None))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(download(bytes))
}

/// `POST /v1/backup` with the passphrase in `x-backup-passphrase`: the sign-ins locked in.
pub(crate) async fn post_backup(
    State(s): State<Arc<NodeState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let pass = passphrase_of(&headers);
    let (bytes, _) = tokio::task::spawn_blocking(move || make(&s, pass.as_deref()))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(download(bytes))
}

#[derive(Debug, Deserialize)]
pub struct RestoreQuery {
    #[serde(default)]
    pub skip_secrets: bool,
    /// Only read the file and say what is in it.
    #[serde(default)]
    pub check: bool,
}

/// What a backup holds, before restoring it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupInfo {
    pub from_node: String,
    pub made_at: i64,
    pub counts: BackupCounts,
    pub locked_signins: bool,
    pub left_out: Vec<String>,
    pub folders: Vec<String>,
}

/// `POST /v1/restore?check=&skip_secrets=` with the file as the body.
pub(crate) async fn post_restore(
    State(s): State<Arc<NodeState>>,
    axum::extract::Query(q): axum::extract::Query<RestoreQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if q.check {
        let p = parse(&body)?;
        return Ok(Json(BackupInfo {
            from_node: p.manifest.node_name,
            made_at: p.manifest.created_at,
            counts: p.manifest.counts,
            locked_signins: p.manifest.secrets.is_some(),
            left_out: p.manifest.left_out,
            folders: p.manifest.folders,
        })
        .into_response());
    }
    let report = restore(&s, body.to_vec(), passphrase_of(&headers), q.skip_secrets).await?;
    Ok(Json(report).into_response())
}

/// `GET /v1/backup/schedule`.
pub(crate) async fn get_schedule(State(s): State<Arc<NodeState>>) -> ApiResult<ScheduleView> {
    let sch = load_schedule(&s);
    let files = if sch.folder.is_empty() {
        Vec::new()
    } else {
        backups_in(Path::new(&sch.folder))
    };
    Ok(Json(ScheduleView {
        folder: sch.folder,
        every_hours: sch.every_hours,
        keep: sch.keep,
        with_signins: sch.passphrase.is_some(),
        last_at: sch.last_at,
        last_file: sch.last_file,
        last_error: sch.last_error,
        files,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ScheduleUpdate {
    pub folder: String,
    pub every_hours: Option<u32>,
    pub keep: Option<u32>,
    /// `Some("")` forgets the passphrase; absent keeps it.
    pub passphrase: Option<String>,
}

/// `PUT /v1/backup/schedule {folder, every_hours?, keep?, passphrase?}`; an empty folder turns it off.
pub(crate) async fn put_schedule(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<ScheduleUpdate>,
) -> ApiResult<ScheduleView> {
    let mut sch = load_schedule(&s);
    let folder = u.folder.trim().to_string();
    if !folder.is_empty() {
        let p = Path::new(&folder);
        if !p.is_absolute() {
            return Err(ApiError::bad_request(
                "the backup folder must be a full path on the node",
            ));
        }
        std::fs::create_dir_all(p).map_err(|e| ApiError::bad_request(format!("{folder}: {e}")))?;
    }
    sch.folder = folder;
    sch.every_hours = u.every_hours.unwrap_or(sch.every_hours).clamp(1, 24 * 30);
    sch.keep = u.keep.unwrap_or(sch.keep).clamp(1, 365);
    match u.passphrase {
        Some(p) if p.is_empty() => sch.passphrase = None,
        Some(p) => sch.passphrase = Some(p),
        None => {}
    }
    save_schedule(&s, &sch)?;
    get_schedule(State(s)).await
}

/// `POST /v1/backup/schedule/run`: one scheduled backup now.
pub(crate) async fn run_schedule(State(s): State<Arc<NodeState>>) -> ApiResult<ScheduleView> {
    run_scheduled(&s).await.map_err(ApiError::bad_request)?;
    get_schedule(State(s)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_seal_and_open_only_with_the_passphrase() {
        let mut sec = Secrets::default();
        sec.settings
            .insert("tidal.user".into(), "{\"token\":\"t\"}".into());
        sec.config = Some("[tidal]\nclient_secret = \"x\"\n".into());
        let (sealed, data) = seal(&sec, "correct horse");
        assert_eq!(open_sealed(&sealed, &data, "correct horse").unwrap(), sec);
        assert!(matches!(
            open_sealed(&sealed, &data, "wrong"),
            Err(BackupError::WrongPassphrase)
        ));
        assert!(!String::from_utf8_lossy(&data).contains("client_secret"));
    }

    #[test]
    fn old_backups_are_pruned_to_the_newest_n() {
        let dir = tempfile::tempdir().unwrap();
        for d in 1..=5 {
            std::fs::write(
                dir.path()
                    .join(format!("pubky-music-2026-10-0{d}-120000.pmbackup")),
                b"x",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("notes.txt"), b"keep me").unwrap();
        let removed = prune(dir.path(), 3);
        assert_eq!(removed.len(), 2);
        assert_eq!(
            backups_in(dir.path()),
            vec![
                "pubky-music-2026-10-05-120000.pmbackup",
                "pubky-music-2026-10-04-120000.pmbackup",
                "pubky-music-2026-10-03-120000.pmbackup"
            ]
        );
        assert!(dir.path().join("notes.txt").exists());
    }
}
