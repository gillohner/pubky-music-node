//! Settings › Accounts: the sources vault on the owner's Pubky account (STEWARD-VAULT.md item 5).
//! The device list with last-used times, "Sign out this device" and "Sign out everywhere". The
//! vault itself (encryption, device proofs, locks) is `pm_pubky::vault`; owner only.
//!
//! The service side of a sign-out (a forced refresh or a token revocation, where the service
//! allows it) comes with each source's vault integration. Until then the answer says so: a removed
//! device can't read anything written after it, but tokens it read before stay valid at the
//! service until they expire.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use pm_pubky::vault::{Device, SignOutReport, Vault, VaultStoreError};
use pubky_music_specs::vault::{ClientKey, ClientSecret};
use serde::Serialize;
use std::sync::Arc;

/// Where the node keeps its vault key: `<data>/vault/client.key` (0600).
pub(crate) fn client_secret(s: &NodeState) -> Result<ClientSecret, ApiError> {
    let path = s.data_dir().join("vault").join("client.key");
    pm_pubky::vault::load_or_create_client_secret(&path).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the node's vault key: {e}"),
        )
    })
}

fn signed_in(s: &NodeState) -> Result<Arc<crate::sync::PubkySync>, ApiError> {
    s.sync
        .lock()
        .expect("sync lock")
        .clone()
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "sign the node in with Pubky Ring first"))
}

fn vault_error(e: VaultStoreError) -> ApiError {
    let status = match &e {
        VaultStoreError::Busy(_) => StatusCode::CONFLICT,
        VaultStoreError::Revoked | VaultStoreError::NoGrant | VaultStoreError::NotOurs(_) => {
            StatusCode::CONFLICT
        }
        VaultStoreError::BadService(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    ApiError::new(status, e.to_string())
}

/// One sign-in, as Settings lists it: never a token.
#[derive(Debug, Serialize)]
pub struct VaultSignIn {
    pub service: String,
    /// This node can open it.
    pub readable: bool,
    pub epoch: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub way: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<u64>,
}

/// `GET /v1/vault`.
#[derive(Debug, Serialize)]
pub struct VaultView {
    /// The node's own vault key (hex).
    pub this_device: String,
    /// Listed and trusted in the device list.
    pub registered: bool,
    pub devices: Vec<Device>,
    pub sign_ins: Vec<VaultSignIn>,
}

async fn view(vault: &Vault<'_>) -> Result<VaultView, VaultStoreError> {
    let devices = vault.devices().await?;
    let me = vault.client_key().to_hex();
    let registered = devices.iter().any(|d| d.this_node && d.trusted);
    let mut sign_ins = Vec::new();
    for service in vault.services().await? {
        let env = vault.envelope(&service).await?;
        match vault.read(&service).await {
            Ok(Some(o)) => sign_ins.push(VaultSignIn {
                service,
                readable: true,
                epoch: o.epoch,
                way: o.sign_in.way.clone(),
                expires_at: o.sign_in.expires_at,
                last_refresh_at: o.sign_in.last_refresh.map(|l| l.at),
            }),
            _ => sign_ins.push(VaultSignIn {
                service,
                readable: false,
                epoch: env.map_or(0, |e| e.epoch),
                way: None,
                expires_at: None,
                last_refresh_at: None,
            }),
        }
    }
    Ok(VaultView {
        this_device: me,
        registered,
        devices,
        sign_ins,
    })
}

/// `GET /v1/vault`: the device list (trusted or not, and why), and the sign-ins this node can read.
pub(crate) async fn get(State(s): State<Arc<NodeState>>) -> ApiResult<VaultView> {
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    let vault = Vault::new(sync.session(), &secret).device("node");
    Ok(Json(view(&vault).await.map_err(vault_error)?))
}

/// `POST /v1/vault/register`: put the node in the device list (under its name) and give the
/// devices that joined since the sign-ins it can read.
pub(crate) async fn register(State(s): State<Arc<NodeState>>) -> ApiResult<VaultView> {
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    let vault = Vault::new(sync.session(), &secret).device("node");
    vault.touch(&s.name).await.map_err(vault_error)?;
    vault.share_with_trusted().await.map_err(vault_error)?;
    Ok(Json(view(&vault).await.map_err(vault_error)?))
}

/// `POST /v1/vault/devices/{key}/sign-out`.
#[derive(Debug, Serialize)]
pub struct SignedOut {
    #[serde(flatten)]
    pub report: SignOutReport,
    /// Sign-ins whose tokens were also revoked or refreshed at the service: none yet (each
    /// source's vault integration adds its own).
    pub refreshed_at_service: Vec<String>,
}

/// `POST /v1/vault/devices/{key}/sign-out`: "Sign out this device". It can't read what is written
/// from now on; the node itself can't be signed out here.
pub(crate) async fn sign_out_device(
    State(s): State<Arc<NodeState>>,
    Path(key): Path<String>,
) -> ApiResult<SignedOut> {
    let device = ClientKey::from_hex(&key).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    if device == secret.public() {
        return Err(ApiError::bad_request(
            "this is the node itself: use Sign out everywhere, or sign the node out of Pubky",
        ));
    }
    let vault = Vault::new(sync.session(), &secret).device("node");
    let report = vault
        .sign_out_device(&device, |service, _| async move {
            tracing::info!(service, "vault: rotated past a signed-out device");
        })
        .await
        .map_err(vault_error)?;
    Ok(Json(SignedOut {
        report,
        refreshed_at_service: Vec::new(),
    }))
}

/// `POST /v1/vault/sign-out-everywhere`: delete every sign-in from the vault (the devices stay
/// listed and can take new sign-ins).
pub(crate) async fn sign_out_everywhere(State(s): State<Arc<NodeState>>) -> ApiResult<serde_json::Value> {
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    let vault = Vault::new(sync.session(), &secret).device("node");
    let removed = vault
        .sign_out_everywhere(|service, _| async move {
            tracing::info!(service, "vault: sign-in removed");
        })
        .await
        .map_err(vault_error)?;
    // The node's own copies go aside now, not at the next sync
    let cache = crate::vault_cache::sync(&s, &vault).await.map_err(vault_error)?;
    Ok(Json(serde_json::json!({
        "removed": removed, "revoked_at_service": [], "signed_out_here": cache.signed_out
    })))
}

/// `POST /v1/vault/sync`: register, share, and sync the node's sign-ins with the vault now (it
/// also runs after every sign-in and every 15 minutes). Answers what moved, by service.
pub(crate) async fn sync_now(State(s): State<Arc<NodeState>>) -> ApiResult<crate::vault_cache::CacheReport> {
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    let vault = Vault::new(sync.session(), &secret);
    Ok(Json(
        crate::vault_cache::pass(&s, &vault).await.map_err(vault_error)?,
    ))
}
