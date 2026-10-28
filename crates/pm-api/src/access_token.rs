//! `POST /v1/node/token`: the owner replaces the node's API token. Only a token the node made
//! itself (`<data_dir>/api-token`) can be replaced here; one set in the config (`[api] token`,
//! `PM_TOKEN`) is changed there. The old token stops working at once, the new one is in the
//! answer (shown once) and in the file, and the node record on the owner's homeserver is
//! published again so their signed-in devices find it.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, serde::Serialize)]
pub struct NewToken {
    /// The token every app and browser needs from now on.
    pub token: String,
}

/// `POST /v1/node/token`.
pub async fn replace(State(s): State<Arc<NodeState>>) -> ApiResult<NewToken> {
    let Some(file) = s.token_file().map(Path::to_path_buf) else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this node's token is set in its config ([api] token, or PM_TOKEN in Docker): change it there and restart the node",
        ));
    };
    let token = new_token();
    let t = token.clone();
    tokio::task::spawn_blocking(move || write_token(&file, &t))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the new token could not be saved: {e}"),
            )
        })?;
    *s.token.write().expect("token lock") = Some(token.clone());
    tracing::info!("API token replaced by the owner; the old one no longer works");
    if let Some(sync) = s.sync_handle() {
        let st = s.clone();
        tokio::spawn(async move {
            if let Err(e) = sync.publish_node_for(&st).await {
                tracing::warn!("publishing the node record with the new token failed: {e}");
            }
        });
    }
    Ok(Json(NewToken { token }))
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
    let tmp = path.with_extension(format!("new{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
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
    r
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
