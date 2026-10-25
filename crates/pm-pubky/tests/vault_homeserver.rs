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

    // A and B register; both are trusted and each sees the other
    v[0].register("Living room node").await.unwrap();
    v[1].register("Phone").await.unwrap();
    v[1].touch("Phone").await.unwrap(); // fresh: nothing to do
    let ix = v[0].index().await.unwrap();
    assert_eq!(ix.clients.len(), 2);
    let trusted: Vec<_> = v[1].trusted(&ix).into_iter().map(|t| t.client_key).collect();
    assert_eq!(trusted, [v[0].client_key(), v[1].client_key()]);
    let devices = v[0].devices().await.unwrap();
    assert!(devices.iter().all(|d| d.trusted), "{devices:?}");
    assert_eq!(devices.iter().filter(|d| d.this_node).count(), 1);

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

/// A service that rotates refresh tokens, as TIDAL and Spotify do: each refresh token works once,
/// so two clients refreshing the same copy would leave one of them signed out.
struct RotatingService {
    valid: std::sync::Mutex<String>,
    calls: std::sync::atomic::AtomicUsize,
    takes: std::time::Duration,
}

impl RotatingService {
    fn new(first: &str, takes: std::time::Duration) -> Self {
        Self {
            valid: std::sync::Mutex::new(first.into()),
            calls: Default::default(),
            takes,
        }
    }

    async fn refresh(&self, mut s: SignIn) -> Result<SignIn, pm_pubky::vault::RefreshFailed> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        tokio::time::sleep(self.takes).await;
        let mut valid = self.valid.lock().unwrap();
        if s.tokens["refresh_token"].as_str() != Some(valid.as_str()) {
            return Err(pm_pubky::vault::RefreshFailed("invalid_grant".into()));
        }
        *valid = format!("rt-{n}");
        s.tokens.insert("refresh_token".into(), json!(format!("rt-{n}")));
        s.tokens.insert("access_token".into(), json!(format!("at-{n}")));
        s.expires_at = Some(now() + 3600);
        Ok(s)
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn expired() -> SignIn {
    serde_json::from_value(json!({
        "service": "tidal", "tokens": {"access_token": "at-0", "refresh_token": "rt-0"},
        "expires_at": now() - 10
    }))
    .unwrap()
}

/// Two clients find the same expired token at the same moment: one refreshes, the other uses
/// what it wrote (STEWARD-VAULT.md items 4 and 7), with the homeserver's lock or the lease file.
async fn two_clients_refresh_at_once(lease_only: bool, takes: std::time::Duration) {
    use pm_pubky::vault::Refreshed;
    let Some(a) = admin() else {
        eprintln!("PM_BUNDLED_HS_ADMIN / PM_BUNDLED_HS_PASSWORD not set: skipped");
        return;
    };
    let (_signer, sessions) = person_with_clients(&a, 2).await;
    let (ka, kb) = (ClientSecret::generate(), ClientSecret::generate());
    let (mut va, mut vb) = (Vault::new(&sessions[0], &ka), Vault::new(&sessions[1], &kb));
    if lease_only {
        (va, vb) = (va.lease_only(), vb.lease_only());
    }
    va.write("tidal", &expired(), &[vb.client_key()]).await.unwrap();
    let svc = RotatingService::new("rt-0", takes);
    let (ra, rb) = tokio::join!(
        va.refresh("tidal", false, |s| svc.refresh(s)),
        vb.refresh("tidal", false, |s| svc.refresh(s)),
    );
    let (ra, rb) = (ra.expect("A"), rb.expect("B"));
    assert_eq!(svc.calls(), 1, "only one client asks the service");
    let ours = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, Refreshed::ByUs(_)))
        .count();
    assert_eq!(ours, 1, "{ra:?} / {rb:?}");
    // Both now hold the one valid token, and the vault has it too
    assert_eq!(ra.sign_in().tokens["refresh_token"], "rt-1");
    assert_eq!(rb.sign_in().tokens["refresh_token"], "rt-1");
    let stored = vb.read("tidal").await.unwrap().unwrap().sign_in;
    assert_eq!(stored.tokens["refresh_token"], "rt-1");
    assert_eq!(stored.revision, 2);
    assert!(stored.last_refresh.is_some());
    // Fresh now: nobody calls the service
    assert!(matches!(
        va.refresh("tidal", false, |s| svc.refresh(s)).await.unwrap(),
        Refreshed::Current(_)
    ));
    assert_eq!(svc.calls(), 1);
    // The service refuses a token (force): the refused copy isn't written over the vault's
    svc.valid
        .lock()
        .unwrap()
        .replace_range(.., "revoked-at-the-service");
    assert!(matches!(
        va.refresh("tidal", true, |s| svc.refresh(s)).await,
        Err(VaultStoreError::Refresh(e)) if e == "invalid_grant"
    ));
    assert_eq!(
        va.read("tidal").await.unwrap().unwrap().sign_in.tokens["refresh_token"],
        "rt-1"
    );
    // No lease is left behind
    let lease = sessions[0]
        .storage()
        .get(pubky_music_specs::vault::lease_path("tidal").unwrap().as_str())
        .await;
    assert!(lease.is_err(), "a lease was left behind");
}

#[tokio::test]
async fn two_clients_refresh_at_once_under_the_lock() {
    two_clients_refresh_at_once(false, std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn two_clients_refresh_at_once_with_the_lease_file() {
    two_clients_refresh_at_once(true, std::time::Duration::from_millis(300)).await;
}

/// A slow service: the holder renews its lock while it waits, and the other client still waits.
#[tokio::test]
async fn a_slow_refresh_keeps_its_lock() {
    two_clients_refresh_at_once(false, std::time::Duration::from_secs(12)).await;
}

/// Revocation (item 5): "Sign out this device" rotates past it and keeps it out; a sign-in this
/// node can't read is reported for another device to rotate; "Sign out everywhere" removes them.
#[tokio::test]
async fn signing_a_device_out_and_signing_out_everywhere() {
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
    for (i, name) in ["Node", "Phone", "Old laptop"].iter().enumerate() {
        v[i].register(name).await.unwrap();
    }
    v[0].write_shared("tidal", &sign_in("t")).await.unwrap();
    v[0].write_shared("qobuz", &sign_in("q")).await.unwrap();
    // One A can't read: the phone wrote it for itself and the laptop only
    v[1].write("deezer", &sign_in("d"), &[v[2].client_key()])
        .await
        .unwrap();
    assert!(v[2].read("tidal").await.unwrap().is_some());

    // The node can't sign itself out
    assert!(v[0]
        .sign_out_device(&v[0].client_key(), |_, _| async {})
        .await
        .is_err());

    // Sign the old laptop out
    let called = std::sync::Mutex::new(Vec::new());
    let report = v[0]
        .sign_out_device(&v[2].client_key(), |service, s| {
            called.lock().unwrap().push((service, s.revision));
            async {}
        })
        .await
        .unwrap();
    assert_eq!(report.rotated, ["qobuz", "tidal"]);
    assert_eq!(report.not_readable_here, ["deezer"]);
    assert_eq!(
        called
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>(),
        ["qobuz", "tidal"]
    );
    // It reads nothing written from now on, the phone still does, and it can't come back
    for service in ["tidal", "qobuz"] {
        assert!(v[2].read(service).await.is_err(), "{service}");
        assert_eq!(v[1].read(service).await.unwrap().unwrap().epoch, 2, "{service}");
    }
    assert!(matches!(
        v[2].register("Old laptop").await,
        Err(VaultStoreError::Revoked)
    ));
    let ix = v[0].index().await.unwrap();
    assert_eq!(ix.clients.len(), 2);
    assert!(ix.is_revoked(&v[2].client_key(), None));
    // The phone, which can read deezer, rotates it past the laptop too
    let r = v[1]
        .sign_out_device(&v[2].client_key(), |_, _| async {})
        .await
        .unwrap();
    assert!(r.rotated.contains(&"deezer".to_string()), "{r:?}");
    assert!(v[2].read("deezer").await.is_err());

    // Sign out everywhere: every sign-in goes, the device list stays
    let removed = v[0].sign_out_everywhere(|_, _| async {}).await.unwrap();
    assert_eq!(removed, ["deezer", "qobuz", "tidal"]);
    assert!(v[0].services().await.unwrap().is_empty());
    assert_eq!(v[0].index().await.unwrap().clients.len(), 2);
}
