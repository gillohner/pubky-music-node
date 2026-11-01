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
    /// The folder's own filesystem, or its nearest existing parent's (a folder not made yet).
    fn space(&self) -> Option<Space> {
        self.0.ancestors().find_map(statvfs)
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
/// `needed` counts only what will be sent: files already on the homeserver are skipped, so a copy
/// run again over a library mostly there isn't refused for them.
pub fn plan(
    room: &dyn Room,
    progress: &mut crate::types::UploadProgress,
    needed: u64,
) -> Result<(), crate::ApiError> {
    progress.needed_bytes = Some(needed);
    let Some(space) = room.space() else {
        return Ok(());
    };
    progress.free_bytes = Some(space.free);
    progress.reserve_bytes = Some(space.reserve());
    match space.too_little(needed) {
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

/// macOS's `._*` AppleDouble files: never audio, never copied nor counted in the space needed.
pub fn is_apple_double(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("._"))
}

/// Whether a file is audio the library plays: macOS's `._*` AppleDouble files are not, and the
/// format is probed, not taken from the extension (a truncated or mislabelled file fails it).
pub async fn is_audio(path: &Path) -> bool {
    if is_apple_double(path) {
        return false;
    }
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || pm_library::probe_and_read(&p).is_ok())
        .await
        .unwrap_or(false)
}

/// Deletes what an upload left at `remote` (best effort): the homeserver may have kept part of it.
/// After a failed or short upload: what this attempt may have left at `remote`. A file the
/// homeserver had before the attempt is never touched (it may be the good copy); one this attempt
/// made is deleted when its size isn't what was sent (a complete one whose answer got lost stays).
/// False when that can't be told now (the homeserver didn't answer): the in-flight record stays
/// for the next start ([`settle_in_flight`]).
pub async fn drop_partial(target: &dyn Target, remote: &str, existed: bool, sent: u64) -> bool {
    if existed {
        return true;
    }
    match target.stat(remote).await {
        // 0: the homeserver didn't say how big it is (no Content-Length): never deleted on that
        Ok(Some(kept)) if kept != sent && kept != 0 => {
            if let Err(e) = target.delete(remote).await {
                tracing::warn!(remote, "a partly copied file stays on the homeserver: {e}");
                return false;
            }
            true
        }
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(remote, "couldn't check what a failed upload left: {e}");
            false
        }
    }
}

/// Uploads in progress, kept until settled: a node stopped in the middle of one (a crash, a
/// restart) finds them at the next start and removes what it left half written there.
pub const IN_FLIGHT_KEY: &str = "files.in_flight";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct InFlight {
    size: u64,
    /// The homeserver had a file there before: never deleted.
    existed: bool,
}

fn in_flight(lib: &pm_library::Library) -> std::collections::BTreeMap<String, InFlight> {
    lib.get_setting(IN_FLIGHT_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn note_in_flight(s: &NodeState, remote: &str, entry: Option<InFlight>) {
    let lib = s.library.lock().expect("library poisoned");
    let mut all = in_flight(&lib);
    match entry {
        Some(e) => {
            all.insert(remote.to_string(), e);
        }
        None => {
            all.remove(remote);
        }
    }
    let _ = if all.is_empty() {
        lib.delete_setting(IN_FLIGHT_KEY).map(drop)
    } else {
        lib.set_setting(IN_FLIGHT_KEY, &serde_json::to_string(&all).expect("serializes"))
    };
}

/// At start: the uploads a stopped node left unsettled. One this node made that the homeserver
/// keeps at another size than was sent is deleted; anything else is left as it is.
/// Waits for the homeserver session first (it is restored after the jobs are looked at): up to
/// `tries` × `every`.
pub async fn settle_in_flight(s: &Arc<NodeState>, tries: u32, every: std::time::Duration) {
    if in_flight(&s.library.lock().expect("library poisoned")).is_empty() {
        return;
    }
    let mut target = None;
    for _ in 0..tries {
        if let Ok(t) = target_of(s) {
            target = Some(t);
            break;
        }
        tokio::time::sleep(every).await;
    }
    let Some(target) = target else {
        tracing::warn!("no homeserver session: uploads left unsettled stay recorded for the next start");
        return;
    };
    let all = in_flight(&s.library.lock().expect("library poisoned"));
    for (remote, e) in all {
        // A copy started meanwhile may be sending the same path: only the record as it was
        let unchanged = || in_flight(&s.library.lock().expect("library poisoned")).get(&remote) == Some(&e);
        if !unchanged() {
            continue;
        }
        if drop_partial(target.as_ref(), &remote, e.existed, e.size).await && unchanged() {
            note_in_flight(s, &remote, None);
        }
    }
}

/// What the homeserver has at a path it listed, right before a copy would send over it.
#[derive(Debug, PartialEq)]
pub enum There {
    /// Nothing any more: sent as a new file.
    Gone,
    /// The same size as the source (or a size it doesn't say): kept, not sent.
    Same,
    /// Known to be another size: sent over it.
    Differs,
}

/// The homeserver has no rename, so an upload over a file writes into it, and a failed one leaves
/// a truncated copy where a good one was: a file is only sent over when its size is known to
/// differ. `listed`: the size the listing gave (an empty file listed as 0 is known to differ;
/// a 0 from the check alone is a size the homeserver didn't say).
pub async fn there(
    target: &dyn Target,
    remote: &str,
    len: u64,
    listed: Option<u64>,
) -> Result<There, String> {
    Ok(match target.stat(remote).await? {
        None => There::Gone,
        Some(n) if n == len || (n == 0 && listed != Some(0)) => There::Same,
        Some(_) => There::Differs,
    })
}

/// Why an upload didn't go through.
#[derive(Debug)]
pub enum Refused {
    /// It would eat into the disk's reserve: the copy stops.
    Space(String),
    /// The homeserver is full (507): the copy stops.
    Full,
    /// The homeserver kept another size.
    Short(u64),
    Other(String),
}

/// One file to the homeserver, guarded: the space check right before it with what is really sent,
/// recorded while in flight, checked afterwards, and what a failure left cleaned up as
/// [`drop_partial`] says. `existed`: the homeserver listed a file at `remote` before.
pub async fn upload_guarded(
    s: &NodeState,
    target: &dyn Target,
    room: &dyn Room,
    remote: &str,
    bytes: Vec<u8>,
    rate: Option<u64>,
    existed: bool,
) -> Result<(), Refused> {
    let len = bytes.len() as u64;
    if let Some(why) = room.space().and_then(|sp| sp.too_little(len)) {
        return Err(Refused::Space(why));
    }
    note_in_flight(s, remote, Some(InFlight { size: len, existed }));
    let failed = match target.upload(remote, bytes, rate).await {
        Ok(()) => match target.stat(remote).await {
            // 0: the homeserver didn't say how big it is; counted as sent
            Ok(Some(kept)) if kept != len && kept != 0 => Some(Refused::Short(kept)),
            Ok(None) => Some(Refused::Short(0)),
            Ok(Some(_)) => None,
            // Not known whole: failed, and its record kept for the next start to settle
            Err(e) => {
                return Err(Refused::Other(format!(
                    "couldn't check what the homeserver kept ({e}); run the copy again"
                )))
            }
        },
        Err(UploadError::Full(_)) => Some(Refused::Full),
        Err(UploadError::Other(e)) => Some(Refused::Other(e)),
    };
    match failed {
        None => {
            note_in_flight(s, remote, None);
            Ok(())
        }
        Some(r) => {
            if drop_partial(target, remote, existed, len).await {
                note_in_flight(s, remote, None);
            }
            Err(r)
        }
    }
}

/// The refusal of a file that would eat into the disk's reserve: the copy stops there.
pub fn is_space_stop(e: &str) -> bool {
    e.contains("not enough space on this machine")
}

/// Where a folder import or a browser upload lands on this machine: the bundled homeserver's
/// disk; with another homeserver, the node's own copy of each file in its mirror folder.
pub fn room_for_store(s: &NodeState) -> Arc<dyn Room> {
    if let Some(r) = s.copy_seams.lock().expect("copy seams").1.clone() {
        return r;
    }
    if crate::storage::homeserver_music_dir(s).is_some() {
        return room_of(s);
    }
    Arc::new(Disk(s.mirror_dir.clone()))
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
