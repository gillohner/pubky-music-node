//! The sources vault on a real pubky-homeserver (VAULT.md, STEWARD-VAULT.md item 2): two clients of
//! one person read and write sign-ins that the homeserver only sees encrypted.
//!
//! Needs the homeserver's admin API in `PM_BUNDLED_HS_ADMIN` and `PM_BUNDLED_HS_PASSWORD` (CI's
//! `bundle` job, against `docker compose up`); skipped otherwise. A test signer stands in for
//! Pubky Ring: it creates the account and approves each client's sign-in.

use pm_pubky::homeserver::NodeHomeserver;
use pm_pubky::vault::{Vault, VaultStoreError};
use pubky::{Keypair, Pubky, PublicKey};
use pubky_music_specs::vault::{blob_path, ClientSecret, SignIn, VaultError};
use serde_json::json;

struct Admin {
    url: String,
    password: String,
}

fn admin() -> Option<Admin> {
    Some(Admin {
        url: std::env::var("PM_BUNDLED_HS_ADMIN").ok()?,
        password: std::env::var("PM_BUNDLED_HS_PASSWORD").ok()?,
    })
}

impl Admin {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut r = reqwest::Client::new()
            .request(method, format!("{}{path}", self.url))
            .header("X-Admin-Password", &self.password);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r
            .send()
            .await
            .expect("admin API")
            .error_for_status()
            .expect("admin API answers");
        let text = resp.text().await.unwrap();
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
    }
}

/// A person on the homeserver, and `n` clients they approved (each its own grant session).
async fn person_with_clients(a: &Admin, n: usize) -> (pubky::PubkySigner, Vec<pubky::PubkySession>) {
    let info = a.call(reqwest::Method::GET, "/info", None).await;
    let hs = PublicKey::try_from(info["public_key"].as_str().expect("homeserver key")).unwrap();
    let token = a
        .call(
            reqwest::Method::POST,
            "/generate_signup_token",
            Some(json!({"storage_quota_mb": "unlimited"})),
        )
        .await;
    let signer = Pubky::new().unwrap().signer(Keypair::random());
    signer
        .signup(&hs, Some(token.as_str().expect("a signup token")))
        .await
        .expect("sign-up");
    let node = NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap();
    let mut sessions = Vec::new();
    for i in 0..n {
        let flow = node.start_login(&format!("vault-test-{i}.pubky.music")).unwrap();
        signer.approve_auth(flow.url()).await.expect("approve");
        sessions.push(flow.await_approval().await.expect("session"));
    }
    (signer, sessions)
}

fn sign_in(token: &str) -> SignIn {
    serde_json::from_value(json!({
        "service": "tidal", "way": "app", "label": "listener@example.org",
        "tokens": {"access_token": token, "refresh_token": format!("{token}-refresh")},
        "expires_at": 1761403600, "settings": {"quality": "lossless"}
    }))
    .unwrap()
}

#[tokio::test]
async fn two_clients_share_a_sign_in_the_homeserver_only_sees_encrypted() {
    let Some(a) = admin() else {
        eprintln!("PM_BUNDLED_HS_ADMIN / PM_BUNDLED_HS_PASSWORD not set: skipped");
        return;
    };
    let (_signer, sessions) = person_with_clients(&a, 2).await;
    let (ka, kb) = (ClientSecret::generate(), ClientSecret::generate());
    let va = Vault::new(&sessions[0], &ka);
    let vb = Vault::new(&sessions[1], &kb);
    assert_eq!(va.owner(), vb.owner());

    // Nothing yet
    assert!(va.read("tidal").await.unwrap().is_none());
    assert!(va.services().await.unwrap().is_empty());

    // A writes, wrapped to itself and B; B reads it
    assert_eq!(
        va.write("tidal", &sign_in("secret-access-1"), &[vb.client_key()])
            .await
            .unwrap(),
        1
    );
    let b_sees = vb.read("tidal").await.unwrap().expect("B opens it");
    assert_eq!(b_sees.sign_in.tokens["access_token"], "secret-access-1");
    assert_eq!(b_sees.sign_in.settings["quality"], "lossless");
    assert_eq!(b_sees.sign_in.created_by, Some(va.client_key()));
    assert_eq!(b_sees.epoch, 1);

    // What the homeserver holds is ciphertext only
    let raw = sessions[0]
        .storage()
        .get(blob_path("tidal").unwrap().as_str())
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for plain in ["secret-access-1", "listener@example.org", "lossless", "refresh"] {
        assert!(
            !raw.contains(plain),
            "{plain} is readable on the homeserver: {raw}"
        );
    }

    // B refreshes: same key and epoch while nobody is dropped, revision goes up
    assert_eq!(
        vb.write("tidal", &sign_in("secret-access-2"), &[va.client_key()])
            .await
            .unwrap(),
        2
    );
    let a_sees = va.read("tidal").await.unwrap().unwrap();
    assert_eq!((a_sees.epoch, a_sees.sign_in.revision), (1, 2));
    assert_eq!(a_sees.sign_in.tokens["access_token"], "secret-access-2");
    assert_eq!(
        a_sees.sign_in.created_by,
        Some(va.client_key()),
        "created_by stays"
    );

    // A drops B: the key rotates and B can't read what comes next
    assert_eq!(
        va.write("tidal", &sign_in("secret-access-3"), &[]).await.unwrap(),
        3
    );
    let after = va.read("tidal").await.unwrap().unwrap();
    assert_eq!((after.epoch, after.recipients.len()), (2, 1));
    match vb.read("tidal").await {
        Err(VaultStoreError::Vault(VaultError::NotARecipient)) => {}
        other => panic!("B still reads the vault: {other:?}"),
    }
    // ...and B, unable to read it, doesn't overwrite it
    match vb.write("tidal", &sign_in("from-b"), &[]).await {
        Err(VaultStoreError::NotOurs(s)) => assert_eq!(s, "tidal"),
        other => panic!("B overwrote a sign-in it can't read: {other:?}"),
    }
    assert_eq!(
        va.read("tidal").await.unwrap().unwrap().sign_in.tokens["access_token"],
        "secret-access-3"
    );

    // A second service, the listing, deleting
    va.write("qobuz", &sign_in("q"), &[]).await.unwrap();
    assert_eq!(va.services().await.unwrap(), ["qobuz", "tidal"]);
    va.delete("qobuz").await.unwrap();
    va.delete("qobuz").await.unwrap(); // nothing to delete is fine
    assert_eq!(va.services().await.unwrap(), ["tidal"]);
    assert!(matches!(
        va.write("../index", &sign_in("x"), &[]).await,
        Err(VaultStoreError::BadService(_))
    ));
}
