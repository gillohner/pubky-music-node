//! `POST /v1/node/token`: the owner replaces the node's API token. Only a token the node made
//! itself (`<data_dir>/api-token`) can be replaced here; one set in the config (`[api] token`,
//! `PM_TOKEN`) is changed there. The old token stops working at once: requests with it are
//! refused and the event sockets opened with it are closed. The new one is in the answer (shown
//! once) and in the file, and the node record on the owner's homeserver is published again so
//! their signed-in devices find it. A backup made before the replacement doesn't bring the old
//! token back ([`REPLACED_FILE`]).

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

/// Beside the token file: when the owner last replaced the token (unix seconds). A restore of a
/// backup made before then keeps the current token (`backup.rs`).
pub const REPLACED_FILE: &str = "api-token.replaced";

/// One replacement at a time, from the write of the file to the swap in memory, so the file and
/// the token the node checks never differ.
static REPLACING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, serde::Serialize)]
pub struct NewToken {
    /// The token every app and browser needs from now on.
    pub token: String,
}

/// `POST /v1/node/token`.
pub async fn replace(State(s): State<Arc<NodeState>>) -> ApiResult<NewToken> {
    let token = rotate(&s).await?;
    if let Some(sync) = s.sync_handle() {
        let st = s.clone();
        tokio::spawn(async move {
            if let Err(e) = sync.publish_node_for(&st).await {
                tracing::warn!("publishing the node record after the token change failed: {e}");
            }
        });
    }
    Ok(Json(NewToken { token }))
}

/// A setting: the SHA-256 of an API token an older version wrote into the node record on the
/// owner's homeserver. Kept from before the record is rewritten until that token no longer works
/// here, so a failure or a restart in between never forgets it (`retire_published`).
pub const PUBLISHED_TOKEN_KEY: &str = "api.published_token";

fn sha256_hex(t: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(t.as_bytes()))
}

fn published_hash(s: &NodeState) -> Option<String> {
    let lib = s.library.lock().expect("library poisoned");
    lib.get_setting(PUBLISHED_TOKEN_KEY).ok().flatten()
}

/// Notes that the node record held `token`, before the record is rewritten without it.
pub(crate) fn note_published(s: &NodeState, token: &str) -> Result<(), pm_library::LibraryError> {
    let lib = s.library.lock().expect("library poisoned");
    lib.set_setting(PUBLISHED_TOKEN_KEY, &sha256_hex(token))
}

/// A token the node record once held stops working here:
/// - the node's token is still that one and the node made it: it is replaced (once; the owner's
///   browsers and apps sign in to the node again), and the note goes;
/// - the node's token is another one by now (replaced, or changed in the config): the note goes;
/// - it is still that one and set in the config, which the node can't change: the note stays and
///   Health asks the owner to change it ([`config_token_published`]).
///
/// A failed replacement keeps the note; the next publish tries again. Callers serialize it.
pub(crate) async fn retire_published(s: &NodeState) {
    let Some(published) = published_hash(s) else {
        return;
    };
    let current = s.token().map(|t| sha256_hex(&t));
    if current.as_deref() != Some(published.as_str()) {
        let lib = s.library.lock().expect("library poisoned");
        let _ = lib.delete_setting(PUBLISHED_TOKEN_KEY);
        return;
    }
    if s.token_file().is_none() {
        tracing::warn!(
            "an older version kept this node's API token in its address record on the homeserver, and it is set in the node's config: change [api] token (or PM_TOKEN) and restart the node"
        );
        return;
    }
    match rotate(s).await {
        Ok(_) => {
            let lib = s.library.lock().expect("library poisoned");
            let _ = lib.delete_setting(PUBLISHED_TOKEN_KEY);
            tracing::warn!(
                "an older version kept this node's API token in its address record on the homeserver; the token was replaced, so apps and browsers sign in to the node again"
            );
        }
        Err(e) => tracing::warn!(
            "replacing the API token an older version kept on the homeserver failed (tried again at the next publish): {}",
            e.message
        ),
    }
}

/// Whether the node still uses a token set in its config that the node record once held.
pub(crate) fn config_token_published(s: &NodeState) -> bool {
    if s.token_file().is_some() {
        return false;
    }
    match (s.token(), published_hash(s)) {
        (Some(t), Some(h)) => sha256_hex(&t) == h,
        _ => false,
    }
}

/// A new API token, in the file and in memory: the old one stops working at once, the owner's
/// browser sessions end and the event sockets opened with it close. Only a token the node made.
pub(crate) async fn rotate(s: &NodeState) -> Result<String, ApiError> {
    let Some(file) = s.token_file().map(Path::to_path_buf) else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this node's token is set in its config ([api] token, or PM_TOKEN in Docker): change it there and restart the node",
        ));
    };
    let _one = REPLACING.lock().await;
    // A new token is the answer to a leak: the owner's browser sessions end with the old token.
    // First, so a failure here leaves the old token working instead of a 500 without the new one
    crate::accounts::end_owner_sessions(s)?;
    // ...and what said the old one was exposed is done with
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(crate::accounts::API_TOKEN_EXPOSED)?;
    let token = new_token();
    let t = token.clone();
    tokio::task::spawn_blocking(move || {
        write_token(&file, &t)?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        write_token(&file.with_file_name(REPLACED_FILE), &now.to_string())
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the new token could not be saved: {e}"),
        )
    })?;
    *s.token.write().expect("token lock") = Some(token.clone());
    // The owner's usage insights let in with the old token change nothing from now on (usage.rs)
    s.usage.revoke_owner();
    // Event sockets opened with the old token end now
    s.token_changed.send_modify(|n| *n += 1);
    tracing::info!("API token replaced; the old one no longer works");
    Ok(token)
}

/// Whether the token was replaced after `made_at` (unix seconds), by [`REPLACED_FILE`] in
/// `data_dir`.
pub fn replaced_after(data_dir: &Path, made_at: i64) -> bool {
    std::fs::read_to_string(data_dir.join(REPLACED_FILE))
        .ok()
        .and_then(|t| t.trim().parse::<i64>().ok())
        .is_some_and(|at| at >= made_at)
}

fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Written whole to a side file (0600 from the start), then renamed over the old one, so the file
/// always holds one whole token.
fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("new{}", new_token()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let r = o
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(token.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r?;
    // The rename itself survives a crash only once the folder is written too
    if let Some(dir) = path.parent() {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_holds_the_new_token_for_its_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        std::fs::write(&path, "old").unwrap();
        write_token(&path, "fresh").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Nothing left beside it
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(new_token().len(), 64);
        assert_ne!(new_token(), new_token());
    }
}
