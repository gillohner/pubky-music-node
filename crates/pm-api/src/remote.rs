//! Remote access over iroh (ADR 0013). The binary configures it; the tunnel starts when the node
//! has a Pubky session, because only then can the owner's devices learn its key (from the private
//! node record, ADR 0008). Without a session nobody could dial it, so nothing runs.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::state::NodeState;

/// How the binary wants remote access run.
#[derive(Debug, Clone)]
pub struct RemoteConfig {
    /// The node's iroh key (made on first start, mode 600).
    pub key_path: PathBuf,
    pub relays: pm_remote::Relays,
    /// The tunnel's own loopback listener (`peer::serve_remote`), so its requests are known.
    pub target: SocketAddr,
}

#[derive(Default)]
pub struct Remote {
    config: Option<RemoteConfig>,
    tunnel: Option<pm_remote::NodeTunnel>,
}

impl NodeState {
    /// Turn remote access on (the binary calls this at start when `[remote] enabled`).
    pub async fn configure_remote(&self, config: RemoteConfig) {
        self.remote.lock().await.config = Some(config);
    }

    /// Start the tunnel if remote access is configured and not running yet; its key as a string.
    pub async fn ensure_remote(&self) -> Option<String> {
        let mut r = self.remote.lock().await;
        if let Some(t) = &r.tunnel {
            return Some(t.id().to_string());
        }
        let cfg = r.config.clone()?;
        let key = match pm_remote::load_or_create_key(&cfg.key_path) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(path = %cfg.key_path.display(), "remote access off: {e:#}");
                return None;
            }
        };
        match pm_remote::NodeTunnel::start(key, &cfg.relays, cfg.target).await {
            Ok(t) => {
                let id = t.id().to_string();
                tracing::info!(id = %id, "remote access on (iroh)");
                *self.remote_id.write().expect("remote id poisoned") = Some(id.clone());
                r.tunnel = Some(t);
                Some(id)
            }
            Err(e) => {
                tracing::warn!("remote access off: the iroh endpoint did not start: {e:#}");
                None
            }
        }
    }
}
