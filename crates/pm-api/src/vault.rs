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
use pm_pubky::vault::{Device, EverywhereReport, SignOutReport, Vault, VaultStoreError};
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

/// The node's own record of signed-out devices: `<data>/vault/revoked.json` (VAULT.md,
/// "Revocation is remembered by each client"). It only grows.
pub(crate) fn revoked_file(s: &NodeState) -> std::path::PathBuf {
    s.data_dir().join("vault").join("revoked.json")
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
        VaultStoreError::Revoked
        | VaultStoreError::NoGrant
        | VaultStoreError::NeedsVaultGrant
        | VaultStoreError::NotOurs(_) => StatusCode::CONFLICT,
        // The device list is full
        VaultStoreError::Vault(pubky_music_specs::vault::VaultError::TooLarge) => StatusCode::CONFLICT,
        VaultStoreError::BadService(_) => StatusCode::BAD_REQUEST,
        VaultStoreError::Local(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
    /// Why it couldn't be read just now (the homeserver, or a file no client wrote); absent when
    /// it was read, or when only another device can open it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `GET /v1/vault`.
#[derive(Debug, Serialize)]
pub struct VaultView {
    /// The node's own vault key (hex).
    pub this_device: String,
    /// Listed and trusted in the device list.
    pub registered: bool,
    pub devices: Vec<Device>,
    /// Why the device list couldn't be read (junk, or too large); the sign-ins are still listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devices_problem: Option<String>,
    pub sign_ins: Vec<VaultSignIn>,
}

async fn view(vault: &Vault<'_>) -> Result<VaultView, VaultStoreError> {
    let (devices, devices_problem) = match vault.devices().await {
        Ok(d) => (d, None),
        Err(e @ (VaultStoreError::Vault(_) | VaultStoreError::Local(_))) => (Vec::new(), Some(e.to_string())),
        Err(e) => return Err(e),
    };
    let me = vault.client_key().to_hex();
    let registered = devices.iter().any(|d| d.this_node && d.trusted);
    let mut sign_ins = Vec::new();
    for service in vault.services().await? {
        // One sign-in that can't be read doesn't hide the others, or the device list
        let unread = |service: String, epoch: u64, problem: Option<String>| VaultSignIn {
            service,
            readable: false,
            epoch,
            way: None,
            expires_at: None,
            last_refresh_at: None,
            problem,
        };
        let env = match vault.envelope(&service).await {
            Ok(e) => e,
            Err(e) => {
                sign_ins.push(unread(service, 0, Some(e.to_string())));
                continue;
            }
        };
        let epoch = env.as_ref().map_or(0, |e| e.epoch);
        match vault.read(&service).await {
            Ok(Some(o)) => sign_ins.push(VaultSignIn {
                service,
                readable: true,
                epoch: o.epoch,
                way: o.sign_in.way.clone(),
                expires_at: o.sign_in.expires_at,
                last_refresh_at: o.sign_in.last_refresh.map(|l| l.at),
                problem: None,
            }),
            Ok(None) => {}
            Err(VaultStoreError::Vault(pubky_music_specs::vault::VaultError::NotARecipient)) => {
                sign_ins.push(unread(service, epoch, None))
            }
            Err(e) => sign_ins.push(unread(service, epoch, Some(e.to_string()))),
        }
    }
    Ok(VaultView {
        this_device: me,
        registered,
        devices,
        devices_problem,
        sign_ins,
    })
}

/// `GET /v1/vault`: the device list (trusted or not, and why), and the sign-ins this node can read.
pub(crate) async fn get(State(s): State<Arc<NodeState>>) -> ApiResult<VaultView> {
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    let revoked = revoked_file(&s);
    let vault = Vault::new(sync.session(), &secret)
        .device("node")
        .remember_revocations(&revoked);
    Ok(Json(view(&vault).await.map_err(vault_error)?))
}

/// `POST /v1/vault/register`: put the node in the device list (under its name) and give the
/// devices that joined since the sign-ins it can read.
pub(crate) async fn register(State(s): State<Arc<NodeState>>) -> ApiResult<VaultView> {
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    let revoked = revoked_file(&s);
    let vault = Vault::new(sync.session(), &secret)
        .device("node")
        .remember_revocations(&revoked);
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
    /// Always true: the device's Pubky session stays valid until the person revokes it in Pubky
    /// Ring. Until then it can still change or delete vault files, though no device that saw the
    /// sign-out gives it a key again.
    pub revoke_in_ring: bool,
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
    let revoked = revoked_file(&s);
    let vault = Vault::new(sync.session(), &secret)
        .device("node")
        .remember_revocations(&revoked);
    let report = vault
        .sign_out_device(&device, |service, _| async move {
            tracing::info!(service, "vault: rotated past a signed-out device");
        })
        .await
        .map_err(vault_error)?;
    Ok(Json(SignedOut {
        report,
        refreshed_at_service: Vec::new(),
        revoke_in_ring: true,
    }))
}

/// `POST /v1/vault/sign-out-everywhere`: delete every sign-in from the vault (the devices stay
/// listed and can take new sign-ins).
pub(crate) async fn sign_out_everywhere(State(s): State<Arc<NodeState>>) -> ApiResult<Everywhere> {
    let sync = signed_in(&s)?;
    let secret = client_secret(&s)?;
    let revoked = revoked_file(&s);
    let vault = Vault::new(sync.session(), &secret)
        .device("node")
        .remember_revocations(&revoked);
    let report = vault
        .sign_out_everywhere(|service, _| async move {
            tracing::info!(service, "vault: sign-in removed");
        })
        .await
        .map_err(vault_error)?;
    Ok(Json(Everywhere {
        report,
        revoked_at_service: Vec::new(),
    }))
}

/// `POST /v1/vault/sign-out-everywhere`.
#[derive(Debug, Serialize)]
pub struct Everywhere {
    #[serde(flatten)]
    pub report: EverywhereReport,
    /// Tokens also revoked at their service: none yet.
    pub revoked_at_service: Vec<String>,
}

/// `POST /v1/vault/new-key {"confirm": true}`: give the node a new vault key, for a node that was
/// signed out of the vault (its old key is never trusted again). The old key file is kept beside
/// it (`client.key.replaced-<unix s>`); the node registers with the new one, and another device
/// gives it the sign-ins. A node signed out by its grant also needs approving again in Ring.
pub(crate) async fn new_key(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<VaultView> {
    if body.get("confirm") != Some(&serde_json::Value::Bool(true)) {
        return Err(ApiError::bad_request(
            "send {\"confirm\": true}: the node can't read its sign-ins until another device shares them with the new key",
        ));
    }
    let sync = signed_in(&s)?;
    let path = s.data_dir().join("vault").join("client.key");
    if path.exists() {
        let aside = path.with_file_name(format!("client.key.replaced-{}", pm_library::db::now_unix()));
        std::fs::rename(&path, &aside).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the node's vault key: {e}"),
            )
        })?;
    }
    let secret = client_secret(&s)?;
    let revoked = revoked_file(&s);
    let vault = Vault::new(sync.session(), &secret)
        .device("node")
        .remember_revocations(&revoked);
    vault.register(&s.name).await.map_err(vault_error)?;
    Ok(Json(view(&vault).await.map_err(vault_error)?))
}
