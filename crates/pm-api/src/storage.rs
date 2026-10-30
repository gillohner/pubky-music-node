//! Where the music and the user's data are kept, for the one status line that replaces the
//! sync UI (STEWARD-BUNDLED-HOMESERVER.md §7).
//!
//! - **Saving is automatic:** playlists, bookmarks, tags and history go to the homeserver by
//!   themselves, a little after the last change ([`spawn_autosave`]); `GET /v1/storage` says
//!   "saved", "saving N changes" or the error, and `POST /v1/storage/save` retries now.
//! - **The homeserver's music is a library folder:** with the bundled homeserver the node mounts
//!   the homeserver's own files folder read-only (`[homeserver] files_dir`), and the owner's
//!   `/priv/pubky.music/files/` there becomes a watched library folder by itself, so uploads and
//!   imports play from local disk with no second copy. The homeserver keeps files as plain paths
//!   (`<files_dir>/<user>/<path>`); the node checks that with a probe before relying on it.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use pm_pubky::homeserver::PRIVATE_FILES_DIR;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the auto-save looks, and how long changes must sit still before it saves.
const TICK: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_secs(20);
/// Changes waiting longer than this are saved even while more keep coming.
const AT_MOST: Duration = Duration::from_secs(120);
/// Written once and deleted at once; inside the files folder because the node only ever writes
/// there, and not audio, so no scan picks it up.
const PROBE: &str = pm_appdata::paths::FILES_LAYOUT_PROBE;

/// What auto-save last did.
#[derive(Debug, Clone, Default)]
pub struct SaveState {
    pub last_saved_at: Option<i64>,
    pub error: Option<String>,
    /// When the pending count was first seen above zero, and its last value.
    since: Option<(Instant, usize, Instant)>,
    /// The homeserver's files folder was checked: `Some(true)` plain paths, `Some(false)` not.
    pub layout_ok: Option<bool>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Changes not on the homeserver yet: music posts, bookmarks and tags.
pub fn pending(s: &NodeState) -> usize {
    let lib = s.library.lock().expect("library poisoned");
    lib.unsynced_music_posts().map(|v| v.len()).unwrap_or(0)
        + lib.unsynced_bookmarks().map(|v| v.len()).unwrap_or(0)
        + lib.unsynced_tags().map(|v| v.len()).unwrap_or(0)
}

/// The owner's music folder inside the bundled homeserver's files folder.
pub fn homeserver_music_dir(s: &NodeState) -> Option<PathBuf> {
    let root = s.homeserver_files_dir()?;
    let owner = s.sync_handle()?.z32().trim_start_matches("pubky").to_string();
    Some(root.join(owner).join(PRIVATE_FILES_DIR.trim_matches('/')))
}

/// Checks once per session that a file written through the homeserver shows up at its plain path
/// under the files folder.
async fn check_layout(s: &NodeState) -> bool {
    if let Some(ok) = s.storage_save.lock().expect("save").layout_ok {
        return ok;
    }
    let (Some(root), Some(sync)) = (s.homeserver_files_dir(), s.sync_handle()) else {
        return false;
    };
    let nonce = crate::tidal::random_urlsafe(12);
    let owner = sync.z32().trim_start_matches("pubky").to_string();
    let local = root.join(&owner).join(PROBE.trim_start_matches('/'));
    let ok = match sync.upload_private(PROBE, nonce.clone().into_bytes()).await {
        Ok(()) => tokio::fs::read_to_string(&local).await.is_ok_and(|t| t == nonce),
        Err(e) => {
            tracing::warn!("homeserver files check: {e}");
            return false;
        }
    };
    let _ = sync.delete_private(PROBE).await;
    if !ok {
        tracing::warn!(dir = %root.display(), "the homeserver's files are not where the node expects them; its music is not used as a library folder");
    }
    s.storage_save.lock().expect("save").layout_ok = Some(ok);
    ok
}

/// Adds the homeserver's music folder to the library once it exists (after the first upload or
/// import) and the layout check passed; with `rescan`, also scans it when it was already there
/// (new files from an upload or import show at once instead of after the watcher's quiet time).
/// Returns the folder when it is (now) a library folder.
pub async fn ensure_homeserver_folder(s: &Arc<NodeState>, rescan: bool) -> Option<PathBuf> {
    let dir = homeserver_music_dir(s)?;
    if !dir.is_dir() || !check_layout(s).await {
        return None;
    }
    let dir = tokio::fs::canonicalize(&dir).await.ok()?;
    let known = s
        .library
        .lock()
        .expect("library poisoned")
        .list_folders()
        .ok()?
        .into_iter()
        .any(|(p, _)| p == dir);
    if !known {
        s.library
            .lock()
            .expect("library poisoned")
            .add_folder(&dir)
            .ok()?;
        s.watch_more(&dir);
        let _ = s.start_scan(vec![dir.clone()]);
        tracing::info!(dir = %dir.display(), "the homeserver's music is a library folder");
    } else if rescan {
        let _ = s.start_scan(vec![dir.clone()]);
    }
    Some(dir)
}

async fn save_now(s: &Arc<NodeState>) -> Result<(), String> {
    let Some(sync) = s.sync_handle() else {
        return Err("not signed in to a homeserver".into());
    };
    if s.sync_activity.lock().expect("sync activity").running {
        return Ok(());
    }
    match sync.sync(&s.library).await {
        Ok(report) => {
            if report.pulled > 0 {
                s.notify_library();
            }
            sync.carry_on(s, &report);
            let mut st = s.storage_save.lock().expect("save");
            st.last_saved_at = Some(now());
            st.error = report.errors.first().cloned().map(|e| plain_error(s, e));
            Ok(())
        }
        Err(e) => {
            let msg = plain_error(s, e.to_string());
            s.storage_save.lock().expect("save").error = Some(msg.clone());
            Err(msg)
        }
    }
}

/// Saves changes to the homeserver by itself: once the pending count has not moved for
/// `QUIET`, or after `AT_MOST` of steady changes. Also keeps the homeserver's music folder in
/// the library.
pub fn spawn_autosave(s: &Arc<NodeState>) {
    let weak = Arc::downgrade(s);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { return };
            if s.sync_handle().is_none() {
                continue;
            }
            ensure_homeserver_folder(&s, false).await;
            let n = pending(&s);
            let due = {
                let mut st = s.storage_save.lock().expect("save");
                match (n, st.since) {
                    (0, _) => {
                        st.since = None;
                        false
                    }
                    (n, None) => {
                        st.since = Some((Instant::now(), n, Instant::now()));
                        false
                    }
                    (n, Some((first, last_n, changed))) => {
                        let changed = if n != last_n { Instant::now() } else { changed };
                        st.since = Some((first, n, changed));
                        changed.elapsed() >= QUIET || first.elapsed() >= AT_MOST
                    }
                }
            };
            if due {
                let _ = save_now(&s).await;
                s.storage_save.lock().expect("save").since = None;
            }
        }
    });
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HomeserverStorage {
    /// `bundled` (the node's own), `external` (another homeserver) or `none` (not signed in).
    pub mode: String,
    pub user: Option<String>,
    /// The owner's storage quota on the bundled homeserver: MB, `"unlimited"`, or null.
    pub quota_mb: Option<serde_json::Value>,
    /// The owner's files on the homeserver, in bytes: all of them on the bundled one; on
    /// another homeserver, the music files it lists for them (it tells nobody its quota).
    pub used_bytes: Option<u64>,
    /// What `used_bytes` counts: `homeserver files` (bundled) or `your music files there` (another
    /// homeserver, whose quota is unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_from: Option<String>,
    /// When the homeserver last answered that it is full (Unix seconds); cleared by the next
    /// upload that goes through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_at: Option<i64>,
}

/// Where the time of the homeserver's last "full" answer is kept.
const FULL_KEY: &str = "storage.full_at";

/// Note that the homeserver answered "full" (507), or that an upload went through or space
/// was freed again. Kept with the account it was for: another sign-in doesn't inherit it.
pub(crate) fn note_full(s: &NodeState, full: bool) {
    let user = s.sync_handle().map(|h| h.z32().to_string());
    let lib = s.library.lock().expect("library poisoned");
    let _ = match (full, user) {
        (true, Some(u)) => lib.set_setting(FULL_KEY, &format!("{u}:{}", pm_library::db::now_unix())),
        (true, None) => Ok(()),
        (false, _) => lib.delete_setting(FULL_KEY).map(|_| ()),
    };
}

/// A homeserver error as people read it: a full homeserver (507) in plain words, and noted.
pub(crate) fn plain_error(s: &NodeState, msg: String) -> String {
    if msg.contains("507 Insufficient Storage") {
        note_full(s, true);
        crate::add_music::FULL.to_string()
    } else {
        msg
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Saving {
    /// `saved`, `saving`, `error` or `signed_out`.
    pub state: String,
    pub pending: usize,
    pub last_saved_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Place {
    pub path: String,
    pub tracks: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Music {
    /// The homeserver's music folder (bundled), when it is a library folder.
    pub on_homeserver: Option<Place>,
    /// The other library folders: used in place.
    pub folders: Vec<Place>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStatus {
    pub homeserver: HomeserverStorage,
    pub saving: Saving,
    pub music: Music,
    /// The copy, import or bring job running now, if any.
    pub job: Option<crate::types::UploadProgress>,
    /// Uploads from the browser in progress.
    pub uploads: crate::add_music::UploadTotals,
}

fn dir_size(dir: &std::path::Path) -> u64 {
    walk(dir).map(|(_, n)| n).sum()
}

pub(crate) fn walk(dir: &std::path::Path) -> impl Iterator<Item = (PathBuf, u64)> {
    let mut stack = vec![dir.to_path_buf()];
    let mut out = Vec::new();
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => out.push((e.path(), e.metadata().map(|m| m.len()).unwrap_or(0))),
                _ => {}
            }
        }
    }
    out.into_iter()
}

/// `GET /v1/storage`.
pub(crate) async fn status(State(s): State<Arc<NodeState>>) -> ApiResult<StorageStatus> {
    let sync = s.sync_handle();
    let bundled = s.bundled_homeserver().is_some();
    let user = sync.as_ref().map(|h| h.z32().to_string());
    let mode = match (&sync, bundled) {
        (None, _) => "none",
        (Some(_), true) if s.homeserver_files_dir().is_some() || bundled => "bundled",
        _ => "external",
    };
    let hs_dir = homeserver_music_dir(&s).filter(|d| d.is_dir());
    let quota_mb = match (&user, bundled) {
        (Some(u), true) => crate::bundled::admin(
            &s,
            reqwest::Method::GET,
            &format!("/users/{}/quota", u.trim_start_matches("pubky")),
            None,
        )
        .await
        .ok()
        .map(|q| q["effective"]["storage_quota_mb"].clone()),
        _ => None,
    };
    let used_bytes = s
        .homeserver_files_dir()
        .zip(user.as_ref())
        .map(|(root, u)| root.join(u.trim_start_matches("pubky")))
        .filter(|d| d.is_dir())
        .map(|d| tokio::task::spawn_blocking(move || dir_size(&d)));
    let used_bytes = match used_bytes {
        Some(h) => h.await.ok(),
        None => None,
    };
    // Another homeserver: the music files it lists for the person (its quota can't be read)
    let (used_bytes, used_from) = match (used_bytes, bundled, &sync) {
        (Some(b), _, _) => (Some(b), Some("homeserver files".to_string())),
        (None, false, Some(h)) => {
            match tokio::time::timeout(std::time::Duration::from_secs(3), h.list_private_files()).await {
                Ok(Ok(files)) => (
                    Some(files.iter().filter_map(|f| f.size).sum()),
                    Some("your music files there".to_string()),
                ),
                _ => (None, None),
            }
        }
        _ => (None, None),
    };
    // Only for the account it was noted for
    let full_at = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(FULL_KEY)
        .ok()
        .flatten()
        .and_then(|v| {
            let (u, at) = v.rsplit_once(':')?;
            (user.as_deref() == Some(u)).then(|| at.parse().ok()).flatten()
        });
    let n = pending(&s);
    let save = s.storage_save.lock().expect("save").clone();
    let running = s.sync_activity.lock().expect("sync activity").running;
    let state = match (&sync, running, n, &save.error) {
        (None, ..) => "signed_out",
        (_, true, ..) => "saving",
        (_, _, _, Some(_)) => "error",
        (_, _, 0, _) => "saved",
        _ => "saving",
    };
    // Tracks and bytes per library folder
    let folders: Vec<PathBuf> = s
        .library
        .lock()
        .expect("library poisoned")
        .list_folders()?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let hs_canon = hs_dir.as_ref().and_then(|d| std::fs::canonicalize(d).ok());
    let mut places = Vec::new();
    let mut on_hs = None;
    for f in folders {
        let (tracks, bytes) = s
            .library
            .lock()
            .expect("library poisoned")
            .folder_totals(&f)
            .unwrap_or((0, 0));
        let p = Place {
            path: f.to_string_lossy().into_owned(),
            tracks,
            bytes,
        };
        if hs_canon.as_ref() == Some(&f) {
            on_hs = Some(p);
        } else {
            places.push(p);
        }
    }
    let job = s
        .upload_jobs
        .lock()
        .expect("upload jobs")
        .values()
        .find(|j| !j.finished)
        .cloned();
    Ok(Json(StorageStatus {
        homeserver: HomeserverStorage {
            mode: mode.into(),
            user,
            quota_mb,
            used_bytes,
            used_from,
            full_at,
        },
        saving: Saving {
            state: state.into(),
            pending: n,
            last_saved_at: save.last_saved_at,
            error: save.error,
        },
        music: Music {
            on_homeserver: on_hs,
            folders: places,
        },
        job,
        uploads: crate::add_music::totals(&s),
    }))
}

/// `POST /v1/storage/save`: save now (the retry button).
pub(crate) async fn save(State(s): State<Arc<NodeState>>) -> ApiResult<StorageStatus> {
    if s.sync_handle().is_none() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "not signed in to a homeserver",
        ));
    }
    save_now(&s)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
    status(State(s)).await
}
