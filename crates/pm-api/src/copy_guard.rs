//! What keeps a copy of library files to the homeserver from hurting the machine it runs on
//! (NUC, 2026-11-01: a folder copy to the bundled homeserver filled the root disk, its Postgres
//! went down for hours, and the copy went on file after file on the homeserver's 500s):
//! - the bundled homeserver's disk keeps a reserve ([`Space::reserve`]): a copy that wouldn't fit
//!   is refused before it starts, and one that runs into the reserve stops before the next file;
//! - a few failures in a row ([`IN_A_ROW`], any kind) stop the job;
//! - only audio goes (`._*` AppleDouble files and anything that doesn't probe as audio stay);
//! - a file the homeserver didn't keep whole is deleted there, never left half written.
//!
//! The homeserver ([`Target`]) and the disk ([`Room`]) are traits so the jobs can be tested
//! without either.

use crate::state::NodeState;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Failures in a row that stop a copy.
pub const IN_A_ROW: usize = 3;
/// Kept free on the bundled homeserver's disk, at least.
pub const MIN_RESERVE: u64 = 5 << 30;

/// Why an upload failed.
#[derive(Debug, Clone)]
pub enum UploadError {
    /// The homeserver said it is full (507).
    Full(String),
    Other(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::Full(e) | UploadError::Other(e) => f.write_str(e),
        }
    }
}

/// Where a copy sends files: the owner's homeserver, through the node's session.
#[async_trait]
pub trait Target: Send + Sync {
    async fn list(&self) -> Result<Vec<pm_pubky::homeserver::RemoteFile>, String>;
    async fn upload(&self, remote: &str, bytes: Vec<u8>, rate: Option<u64>) -> Result<(), UploadError>;
    /// The size the homeserver keeps at `remote`, if any.
    async fn stat(&self, remote: &str) -> Result<Option<u64>, String>;
    async fn delete(&self, remote: &str) -> Result<(), String>;
}

#[async_trait]
impl Target for crate::sync::PubkySync {
    async fn list(&self) -> Result<Vec<pm_pubky::homeserver::RemoteFile>, String> {
        self.list_private_files().await.map_err(|e| e.to_string())
    }
    async fn upload(&self, remote: &str, bytes: Vec<u8>, rate: Option<u64>) -> Result<(), UploadError> {
        self.upload_private_paced(remote, bytes, rate).await.map_err(|e| {
            if e.is_full() {
                UploadError::Full(e.to_string())
            } else {
                UploadError::Other(e.to_string())
            }
        })
    }
    async fn stat(&self, remote: &str) -> Result<Option<u64>, String> {
        self.stat_private(remote).await.map_err(|e| e.to_string())
    }
    async fn delete(&self, remote: &str) -> Result<(), String> {
        self.delete_private(remote).await.map_err(|e| e.to_string())
    }
}

/// A disk's size and what is free on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub free: u64,
    pub total: u64,
}

impl Space {
    /// What stays free for the homeserver and its database: 5 GiB, or 5 % of the disk when more.
    pub fn reserve(&self) -> u64 {
        MIN_RESERVE.max(self.total / 20)
    }

    /// What a copy may still use.
    pub fn usable(&self) -> u64 {
        self.free.saturating_sub(self.reserve())
    }

    /// Why `needed` bytes don't fit, if they don't.
    pub fn too_little(&self, needed: u64) -> Option<String> {
        (needed > self.usable()).then(|| {
            format!(
                "not enough space on this machine: {} needed, {} free ({} kept free for the homeserver)",
                gb(needed),
                gb(self.free),
                gb(self.reserve())
            )
        })
    }
}

fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / 1e9)
}

/// The disk a copy lands on; `None` when the node can't see it (another homeserver).
pub trait Room: Send + Sync {
    fn space(&self) -> Option<Space>;
}

/// A folder's filesystem, read with statvfs.
pub struct Disk(pub PathBuf);

impl Room for Disk {
    fn space(&self) -> Option<Space> {
        statvfs(&self.0)
    }
}

/// No disk to read: another homeserver (its own 507 and the failures in a row stop a copy).
pub struct Unknown;

impl Room for Unknown {
    fn space(&self) -> Option<Space> {
        None
    }
}

#[cfg(unix)]
fn statvfs(path: &Path) -> Option<Space> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a zeroed struct for the call to fill
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some(Space {
        free: st.f_bavail as u64 * frsize,
        total: st.f_blocks as u64 * frsize,
    })
}

#[cfg(not(unix))]
fn statvfs(_: &Path) -> Option<Space> {
    None
}

/// The homeserver a copy goes to: a test's fake when set, else the node's session.
pub fn target_of(s: &NodeState) -> Result<Arc<dyn Target>, crate::ApiError> {
    if let Some(t) = s.copy_seams.lock().expect("copy seams").0.clone() {
        return Ok(t);
    }
    let sync = s.sync_handle().ok_or_else(|| {
        crate::ApiError::new(
            axum::http::StatusCode::PRECONDITION_FAILED,
            "no Pubky session: set `pubky.session_secret_file` in the node config",
        )
    })?;
    Ok(sync)
}

/// Before a copy starts: the planned bytes against the disk it lands on. Fills in the plan's
/// `needed_bytes`, `free_bytes` and `reserve_bytes`; the refusal (507) when they don't fit.
pub fn plan(room: &dyn Room, progress: &mut crate::types::UploadProgress) -> Result<(), crate::ApiError> {
    progress.needed_bytes = Some(progress.bytes_total);
    let Some(space) = room.space() else {
        return Ok(());
    };
    progress.free_bytes = Some(space.free);
    progress.reserve_bytes = Some(space.reserve());
    match space.too_little(progress.bytes_total) {
        Some(why) => Err(crate::ApiError::new(
            axum::http::StatusCode::INSUFFICIENT_STORAGE,
            why,
        )),
        None => Ok(()),
    }
}

/// The disk a copy lands on: the bundled homeserver's files folder, which the node sees.
pub fn room_of(s: &NodeState) -> Arc<dyn Room> {
    if let Some(r) = s.copy_seams.lock().expect("copy seams").1.clone() {
        return r;
    }
    match s.homeserver_files_dir() {
        Some(dir) => Arc::new(Disk(dir.to_path_buf())),
        None => Arc::new(Unknown),
    }
}

/// Failures in a row: the stop reason once there are [`IN_A_ROW`].
#[derive(Debug, Default)]
pub struct Streak(usize);

impl Streak {
    pub fn ok(&mut self) {
        self.0 = 0;
    }

    pub fn failed(&mut self, last: &str) -> Option<String> {
        self.0 += 1;
        (self.0 >= IN_A_ROW).then(|| format!("stopped after {IN_A_ROW} failures in a row; the last: {last}"))
    }
}

/// Whether a file is audio the library plays: macOS's `._*` AppleDouble files are not, and the
/// format is probed, not taken from the extension (a truncated or mislabelled file fails it).
pub async fn is_audio(path: &Path) -> bool {
    if path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("._"))
    {
        return false;
    }
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || pm_library::probe_and_read(&p).is_ok())
        .await
        .unwrap_or(false)
}

/// Deletes what an upload left at `remote` (best effort): the homeserver may have kept part of it.
pub async fn drop_partial(target: &dyn Target, remote: &str) {
    if let Err(e) = target.delete(remote).await {
        tracing::warn!(remote, "a partly copied file stays on the homeserver: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserve_is_5_gib_or_5_percent() {
        let small = Space {
            free: 50 << 30,
            total: 98 << 30,
        };
        assert_eq!(small.reserve(), MIN_RESERVE);
        let big = Space {
            free: 500 << 30,
            total: 2000 << 30,
        };
        assert_eq!(big.reserve(), 100 << 30);
        // 95 % full: 4.9 GiB free, under the reserve
        let full = Space {
            free: (98 << 30) / 20,
            total: 98 << 30,
        };
        assert_eq!(full.usable(), 0);
        let why = full.too_little(1).unwrap();
        assert!(why.starts_with("not enough space on this machine"), "{why}");
        assert!(small.too_little(10 << 30).is_none());
    }

    #[test]
    fn three_failures_in_a_row_stop() {
        let mut s = Streak::default();
        assert!(s.failed("a").is_none());
        assert!(s.failed("b").is_none());
        s.ok();
        assert!(s.failed("c").is_none());
        assert!(s.failed("d").is_none());
        let why = s.failed("e: 500").unwrap();
        assert!(
            why.contains("3 failures in a row") && why.ends_with("e: 500"),
            "{why}"
        );
    }
}
