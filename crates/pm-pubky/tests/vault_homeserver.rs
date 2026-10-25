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

/// The device list (item 3): clients prove their keys with their grants; only trusted entries get
/// keys; a new device is added by one that can read; forged, borrowed and signed-out entries don't.
#[tokio::test]
async fn devices_prove_their_keys_and_only_trusted_ones_get_sign_ins() {
    use pubky_music_specs::vault::{ClientEntry, Index, Proof, Revoked, INDEX_PATH};
    let Some(a) = admin() else {
        eprintln!("PM_BUNDLED_HS_ADMIN / PM_BUNDLED_HS_PASSWORD not set: skipped");
        return;
    };
    let (_signer, sessions) = person_with_clients(&a, 3).await;
    let keys: Vec<ClientSecret> = (0..3).map(|_| ClientSecret::generate()).collect();
    let v: Vec<Vault> = sessions
        .iter()
        .zip(&keys)
        .map(|(s, k)| Vault::new(s, k))
        .collect();

    // A (a node, signing its kind) and B register; both are trusted and each sees the other
    Vault::new(&sessions[0], &keys[0])
        .device("node")
        .register("Living room node")
        .await
        .unwrap();
    v[1].register("Phone").await.unwrap();
    v[1].touch("Phone").await.unwrap(); // fresh: nothing to do
    let ix = v[0].index().await.unwrap();
    assert_eq!(ix.clients.len(), 2);
    let trusted: Vec<_> = v[1].trusted(&ix).into_iter().map(|t| t.client_key).collect();
    assert_eq!(trusted, [v[0].client_key(), v[1].client_key()]);
    let devices = v[0].devices().await.unwrap();
    assert!(devices.iter().all(|d| d.trusted), "{devices:?}");
    assert_eq!(devices.iter().filter(|d| d.this_node).count(), 1);
    // The signed kind, and when each grant expires (for "approve again in Ring")
    assert_eq!(devices[0].device.as_deref(), Some("node"));
    assert_eq!(devices[1].device, None);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        devices
            .iter()
            .all(|d| d.grant_expires_at.is_some_and(|e| e > now)),
        "{devices:?}"
    );

    // A writes for every trusted device; B reads, C (not registered) can't
    v[0].write_shared("tidal", &sign_in("shared-1")).await.unwrap();
    assert_eq!(
        v[1].read("tidal").await.unwrap().unwrap().sign_in.tokens["access_token"],
        "shared-1"
    );
    assert!(v[2].read("tidal").await.is_err());

    // C registers; A adds it to what A can read, keeping the epoch
    v[2].register("Laptop").await.unwrap();
    assert_eq!(v[0].share_with_trusted().await.unwrap(), 1);
    assert_eq!(v[0].share_with_trusted().await.unwrap(), 0, "only once");
    let c_sees = v[2].read("tidal").await.unwrap().unwrap();
    assert_eq!(
        (c_sees.epoch, c_sees.sign_in.tokens["access_token"].as_str()),
        (1, Some("shared-1"))
    );

    // Entries that don't prove themselves get nothing:
    // - another person's client, with its own (valid) grant and proof;
    let (_other, other_sessions) = person_with_clients(&a, 1).await;
    let ok = ClientSecret::generate();
    let ov = Vault::new(&other_sessions[0], &ok);
    ov.register("Someone else's laptop").await.unwrap();
    let foreign: ClientEntry = ov.index().await.unwrap().clients.remove(0);
    // - A's proof copied onto a key A's proof doesn't name;
    let mut borrowed: ClientEntry = ix.clients[0].clone();
    let stranger = ClientSecret::generate();
    borrowed.client_key = stranger.public();
    borrowed.name = "Borrowed".into();
    // - garbage.
    let mut garbage = borrowed.clone();
    garbage.client_key = ClientSecret::generate().public();
    garbage.proof = Proof {
        grant: "x.y.z".into(),
        pop: "a.b.c".into(),
    };
    let mut ix = v[0].index().await.unwrap();
    ix.clients
        .extend([foreign.clone(), borrowed.clone(), garbage.clone()]);
    sessions[0]
        .storage()
        .put(INDEX_PATH, serde_json::to_vec(&ix).unwrap())
        .await
        .unwrap();
    let ix = v[0].index().await.unwrap();
    let trusted: Vec<_> = v[0].trusted(&ix).into_iter().map(|t| t.client_key).collect();
    assert_eq!(trusted, [v[0].client_key(), v[1].client_key(), v[2].client_key()]);
    let devices = v[0].devices().await.unwrap();
    let problem = |k: &pubky_music_specs::vault::ClientKey| {
        devices
            .iter()
            .find(|d| d.client_key == k.to_hex())
            .unwrap()
            .problem
            .clone()
            .unwrap_or_default()
    };
    assert!(
        problem(&foreign.client_key).contains("someone else's"),
        "{}",
        problem(&foreign.client_key)
    );
    assert!(
        problem(&borrowed.client_key).contains("another key"),
        "{}",
        problem(&borrowed.client_key)
    );
    assert!(!problem(&garbage.client_key).is_empty());
    // - a trusted entry renamed on the homeserver ("Your phone" on the laptop): its name isn't
    //   the signed one, so it isn't trusted until the laptop registers again
    let mut ix = v[0].index().await.unwrap();
    let laptop = ix
        .clients
        .iter_mut()
        .find(|e| e.client_key == v[2].client_key())
        .unwrap();
    laptop.name = "Your phone".into();
    sessions[0]
        .storage()
        .put(INDEX_PATH, serde_json::to_vec(&ix).unwrap())
        .await
        .unwrap();
    let devices = v[0].devices().await.unwrap();
    let renamed = devices
        .iter()
        .find(|d| d.client_key == v[2].client_key().to_hex())
        .unwrap();
    assert!(
        !renamed.trusted && renamed.problem.as_deref().unwrap_or_default().contains("name"),
        "{renamed:?}"
    );
    v[2].register("Laptop").await.unwrap();
    assert!(v[0]
        .devices()
        .await
        .unwrap()
        .iter()
        .all(|d| d.client_key != v[2].client_key().to_hex() || d.trusted));
    // ...and share_with_trusted gives them nothing
    assert_eq!(v[0].share_with_trusted().await.unwrap(), 0);
    let env = v[0].envelope("tidal").await.unwrap().unwrap();
    assert_eq!(env.recipients.len(), 3);
    assert!(!env.has_recipient(&stranger.public()));

    // B is signed out (by grant id): no longer trusted, and it can't register again
    let b_jti = v[0]
        .trusted(&ix)
        .into_iter()
        .find(|t| t.client_key == v[1].client_key())
        .unwrap()
        .grant_id;
    let mut ix: Index = v[0].index().await.unwrap();
    ix.revoked.push(Revoked {
        client_key: None,
        grant_id: Some(b_jti),
        at: 1,
    });
    sessions[0]
        .storage()
        .put(INDEX_PATH, serde_json::to_vec(&ix).unwrap())
        .await
        .unwrap();
    let ix = v[0].index().await.unwrap();
    assert!(!v[0]
        .trusted(&ix)
        .iter()
        .any(|t| t.client_key == v[1].client_key()));
    assert!(matches!(
        v[1].register("Phone").await,
        Err(VaultStoreError::Revoked)
    ));
    // The next shared write leaves B out (and rotates, since B held a key)
    v[0].write_shared("tidal", &sign_in("shared-2")).await.unwrap();
    let after = v[0].read("tidal").await.unwrap().unwrap();
    assert_eq!(after.epoch, 2);
    assert!(v[1].read("tidal").await.is_err());
    assert_eq!(
        v[2].read("tidal").await.unwrap().unwrap().sign_in.tokens["access_token"],
        "shared-2"
    );
}

/// mn-app's review of #91: an app the person approved for all their storage (`/:rw`) is not a
/// vault client. It can't register, and an entry it writes itself, with its own valid proof, isn't
/// trusted, so `share_with_trusted` gives it nothing.
#[tokio::test]
async fn a_broad_grant_is_not_a_grant_for_the_sign_ins() {
    use pubky_music_specs::vault::{ClientEntry, ClientProofData, Proof, INDEX_PATH};
    let Some(a) = admin() else {
        eprintln!("PM_BUNDLED_HS_ADMIN / PM_BUNDLED_HS_PASSWORD not set: skipped");
        return;
    };
    let (signer, sessions) = person_with_clients(&a, 1).await;
    let node = NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap();
    let flow = node.start_login_with("backup-tool.example", "/:rw").unwrap();
    signer.approve_auth(flow.url()).await.expect("approve");
    let broad = flow.await_approval().await.expect("session");
    let (kn, kb) = (ClientSecret::generate(), ClientSecret::generate());
    let vn = Vault::new(&sessions[0], &kn);
    vn.register("Node").await.unwrap();
    vn.write_shared("tidal", &sign_in("t")).await.unwrap();

    let vb = Vault::new(&broad, &kb);
    assert!(matches!(
        vb.register("Backup tool").await,
        Err(VaultStoreError::NeedsVaultGrant)
    ));
    // It writes an entry itself, with a proof its grant really signs
    let data = serde_json::to_value(ClientProofData::new(kb.public(), "Backup tool")).unwrap();
    let pop = broad.as_grant().unwrap().create_custom_pop(data).await.unwrap();
    let mut ix = vn.index().await.unwrap();
    ix.clients.push(ClientEntry {
        client_key: kb.public(),
        name: "Backup tool".into(),
        authorised_at: 1,
        last_used_at: 1,
        proof: Proof {
            grant: pop.grant.clone(),
            pop: pop.pop.clone(),
        },
        extra: Default::default(),
    });
    broad
        .storage()
        .put(INDEX_PATH, serde_json::to_vec(&ix).unwrap())
        .await
        .unwrap();
    let ix = vn.index().await.unwrap();
    assert!(!vn.trusted(&ix).iter().any(|t| t.client_key == kb.public()));
    let d = vn.devices().await.unwrap();
    let e = d.iter().find(|d| d.client_key == kb.public().to_hex()).unwrap();
    assert!(
        e.problem
            .as_deref()
            .unwrap_or_default()
            .contains("doesn't name the vault"),
        "{e:?}"
    );
    assert_eq!(vn.share_with_trusted().await.unwrap(), 0);
    assert!(vb.read("tidal").await.is_err());
}
