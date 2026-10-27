//! Settings › Accounts' vault routes (STEWARD-VAULT.md item 5) on a real pubky-homeserver: the
//! device list, registering the node, "Sign out this device" and "Sign out everywhere".
//!
//! Needs the homeserver's admin API in `PM_BUNDLED_HS_ADMIN` and `PM_BUNDLED_HS_PASSWORD` (CI's
//! `bundle` job); skipped otherwise. A test signer stands in for Pubky Ring.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::sync::PubkySync;
use pm_api::{router, NodeSettings, NodeState};
use pm_appdata::vault::{ClientSecret, SignIn};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_pubky::homeserver::NodeHomeserver;
use pm_pubky::vault::Vault;
use pm_zones::OutputRegistry;
use pubky::{Keypair, Pubky, PublicKey};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn admin() -> Option<(String, String)> {
    Some((
        std::env::var("PM_BUNDLED_HS_ADMIN").ok()?,
        std::env::var("PM_BUNDLED_HS_PASSWORD").ok()?,
    ))
}

async fn admin_call(a: &(String, String), method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
    let mut r = reqwest::Client::new()
        .request(method, format!("{}{path}", a.0))
        .header("X-Admin-Password", &a.1);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let text = r
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

fn node(dir: &std::path::Path) -> Arc<NodeState> {
    let db = dir.join("node.sqlite");
    NodeState::new(
        NodeSettings {
            name: "Living room node".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    )
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_device_list_and_signing_out_through_the_api() {
    let Some(a) = admin() else {
        eprintln!("PM_BUNDLED_HS_ADMIN / PM_BUNDLED_HS_PASSWORD not set: skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let app = router(s.clone());

    // Not signed in yet
    assert_eq!(call(&app, "GET", "/v1/vault").await.0, StatusCode::CONFLICT);

    // A person, the node signed in (one grant) and their phone (another)
    let info = admin_call(&a, reqwest::Method::GET, "/info", None).await;
    let hs = PublicKey::try_from(info["public_key"].as_str().unwrap()).unwrap();
    let token = admin_call(
        &a,
        reqwest::Method::POST,
        "/generate_signup_token",
        Some(json!({"storage_quota_mb": "unlimited"})),
    )
    .await;
    let signer = Pubky::new().unwrap().signer(Keypair::random());
    signer.signup(&hs, Some(token.as_str().unwrap())).await.unwrap();
    let homeserver = NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap();
    let mut sessions = Vec::new();
    for id in ["node.pubky.music", "phone.pubky.music"] {
        let flow = homeserver.start_login(id).unwrap();
        signer.approve_auth(flow.url()).await.unwrap();
        sessions.push(flow.await_approval().await.unwrap());
    }
    let phone_session = sessions.pop().unwrap();
    *s.sync.lock().unwrap() = Some(Arc::new(PubkySync::from_session(
        NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap(),
        sessions.pop().unwrap(),
    )));

    // The node registers under its name; its key file is made once, 0600
    let (st, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["registered"], false);
    let (st, v) = call(&app, "POST", "/v1/vault/register").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["registered"], true);
    assert_eq!(v["devices"][0]["name"], "Living room node");
    assert_eq!(v["devices"][0]["this_node"], true);
    assert_eq!(v["devices"][0]["device"], "node");
    let node_key = v["this_device"].as_str().unwrap().to_string();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(dir.path().join("vault/client.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }

    // The phone joins and signs in to TIDAL for everyone; the node lists both and reads it
    let phone_key = ClientSecret::generate();
    let phone = Vault::new(&phone_session, &phone_key);
    phone.register("Phone").await.unwrap();
    let tidal: SignIn = serde_json::from_value(json!({"service": "tidal", "way": "app",
        "tokens": {"access_token": "never-shown", "refresh_token": "never-shown-either"}}))
    .unwrap();
    phone.write_shared("tidal", &tidal).await.unwrap();
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(v["sign_ins"][0]["service"], "tidal");
    assert_eq!(v["sign_ins"][0]["readable"], true);
    assert!(
        !v.to_string().contains("never-shown"),
        "the API shows a token: {v}"
    );

    // The node can't sign itself out; a bad key is refused
    assert_eq!(
        call(&app, "POST", &format!("/v1/vault/devices/{node_key}/sign-out"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, "POST", "/v1/vault/devices/not-a-key/sign-out").await.0,
        StatusCode::BAD_REQUEST
    );

    // The list as it was before the sign-out, with the phone's valid entry
    let before_sign_out = phone.index().await.unwrap();
    // Sign the phone out: TIDAL rotates past it
    let (st, r) = call(
        &app,
        "POST",
        &format!("/v1/vault/devices/{}/sign-out", phone.client_key().to_hex()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["rotated"], json!(["tidal"]));
    assert_eq!(r["refreshed_at_service"], json!([]));
    assert_eq!(r["failed"], json!([]));
    assert_eq!(r["revoke_in_ring"], true);
    assert!(phone.read("tidal").await.is_err());
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["sign_ins"][0]["epoch"], 2);
    // The node remembers the sign-out: the phone writes the list back without it, and the node
    // still doesn't trust it
    assert!(dir.path().join("vault/revoked.json").exists());
    assert!(before_sign_out.revoked.is_empty());
    phone_session
        .storage()
        .put(
            pm_appdata::vault::INDEX_PATH,
            serde_json::to_vec(&before_sign_out).unwrap(),
        )
        .await
        .unwrap();
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    let listed = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["client_key"] == phone.client_key().to_hex())
        .cloned()
        .unwrap_or_else(|| panic!("the phone's entry is back in the list: {v}"));
    assert_eq!(listed["trusted"], false, "{v}");
    // A blob no client wrote doesn't take the device list down
    phone_session
        .storage()
        .put(
            pm_appdata::vault::blob_path("junk").unwrap().as_str(),
            b"junk".to_vec(),
        )
        .await
        .unwrap();
    let (st, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["sign_ins"][0]["problem"].is_string(), "{v}");
    assert_eq!(v["sign_ins"][1]["readable"], true, "{v}");

    // A new vault key: only when confirmed; the old file is kept aside and the node registers
    // with the new key
    assert_eq!(
        post_json(&app, "/v1/vault/new-key", json!({})).await.0,
        StatusCode::BAD_REQUEST
    );
    let (st, v) = post_json(&app, "/v1/vault/new-key", json!({"confirm": true})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_ne!(v["this_device"], json!(node_key));
    assert_eq!(v["registered"], true, "{v}");
    assert!(std::fs::read_dir(dir.path().join("vault")).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("client.key.replaced-")));

    // Sign out everywhere
    let (st, r) = call(&app, "POST", "/v1/vault/sign-out-everywhere").await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["removed"], json!(["junk", "tidal"]));
    assert_eq!(r["left"], json!([]));
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(v["sign_ins"], json!([]));
}

/// Item 6: two nodes of one person. The first one's sign-ins move into the vault; the second
/// gets them once it's shared; a local refresh on either reaches the other; "Sign out
/// everywhere" moves them aside on both, without deleting anything.
#[tokio::test]
async fn two_nodes_keep_their_sign_ins_in_step_through_the_vault() {
    let Some(a) = admin() else {
        eprintln!("PM_BUNDLED_HS_ADMIN / PM_BUNDLED_HS_PASSWORD not set: skipped");
        return;
    };
    let info = admin_call(&a, reqwest::Method::GET, "/info", None).await;
    let hs = PublicKey::try_from(info["public_key"].as_str().unwrap()).unwrap();
    let token = admin_call(
        &a,
        reqwest::Method::POST,
        "/generate_signup_token",
        Some(json!({"storage_quota_mb": "unlimited"})),
    )
    .await;
    let signer = Pubky::new().unwrap().signer(Keypair::random());
    signer.signup(&hs, Some(token.as_str().unwrap())).await.unwrap();
    let homeserver = NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap();
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (n1, n2) = (node(d1.path()), node(d2.path()));
    for (n, id) in [(&n1, "living-room.pubky.music"), (&n2, "kitchen.pubky.music")] {
        let flow = homeserver.start_login(id).unwrap();
        signer.approve_auth(flow.url()).await.unwrap();
        let session = flow.await_approval().await.unwrap();
        *n.sync.lock().unwrap() = Some(Arc::new(PubkySync::from_session(
            NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap(),
            session,
        )));
    }
    let (a1, a2) = (router(n1.clone()), router(n2.clone()));
    let tidal_v1 =
        r#"{"access_token":"at-1","refresh_token":"rt-1","expires_at":1000,"user_id":"7","country":"CH"}"#;
    let set = |n: &NodeState, k: &str, v: &str| n.library.lock().unwrap().set_setting(k, v).unwrap();
    let get = |n: &NodeState, k: &str| n.library.lock().unwrap().get_setting(k).unwrap();
    set(&n1, "tidal.user", tidal_v1);
    set(
        &n1,
        "jamendo.user",
        r#"{"access_token":"jam-1","refresh_token":"jam-r","expires_at":5000,"name":"Gil"}"#,
    );

    // Node 1: its two sign-ins move into the vault
    let (st, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["migrated"], json!(["tidal", "jamendo"]), "{r}");
    // ...and stay where they were on the node
    assert_eq!(get(&n1, "tidal.user").as_deref(), Some(tidal_v1));

    // Node 2 joins: it can't read them until node 1 shares them
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["not_readable"], json!(["tidal", "jamendo"]), "{r}");
    call(&a1, "POST", "/v1/vault/sync").await;
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pulled"], json!(["tidal", "jamendo"]), "{r}");
    assert_eq!(get(&n2, "tidal.user").as_deref(), Some(tidal_v1));
    assert_eq!(
        get(&n2, "jamendo.user").as_deref(),
        Some(r#"{"access_token":"jam-1","refresh_token":"jam-r","expires_at":5000,"name":"Gil"}"#)
    );

    // Node 2's source refreshes TIDAL locally: it reaches node 1
    let tidal_v2 =
        r#"{"access_token":"at-2","refresh_token":"rt-2","expires_at":2000,"user_id":"7","country":"CH"}"#;
    set(&n2, "tidal.user", tidal_v2);
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pushed"], json!(["tidal"]), "{r}");
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pulled"], json!(["tidal"]), "{r}");
    assert_eq!(get(&n1, "tidal.user").as_deref(), Some(tidal_v2));
    // Node 2 refreshes the old account while node 1 signs in to another one: node 1's wins (the
    // person just acted there), and reaches node 2
    let tidal_v3 =
        r#"{"access_token":"at-3","refresh_token":"rt-3","expires_at":3000,"user_id":"7","country":"CH"}"#;
    set(&n2, "tidal.user", tidal_v3);
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pushed"], json!(["tidal"]), "{r}");
    let other = r#"{"access_token":"other-1","refresh_token":"other-r","expires_at":2500,"user_id":"9","country":"DE"}"#;
    set(&n1, "tidal.user", other);
    // (a new sign-in, as the source's own sign-in flow marks it)
    pm_api::vault_cache::new_sign_in(&n1.library.lock().unwrap(), "tidal");
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(
        r["pushed"],
        json!(["tidal"]),
        "node 1's new account goes out: {r}"
    );
    assert_eq!(get(&n1, "tidal.user").as_deref(), Some(other));
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pulled"], json!(["tidal"]), "{r}");
    assert_eq!(get(&n2, "tidal.user").as_deref(), Some(other));
    let tidal_v2 = other;
    // Settings sees the last sync
    let (_, v) = call(&a2, "GET", "/v1/vault").await;
    assert_eq!(v["last_sync"]["report"]["pulled"], json!(["tidal"]), "{v}");
    assert!(v["last_sync"]["at"].as_i64().is_some());
    // Nothing changed: nothing moves
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    let quiet = json!({"migrated": [], "pushed": [], "pulled": [], "signed_out": [], "removed": [],
        "conflicts": [], "not_usable": [], "not_readable": [], "failed": [], "forgotten": [], "details": []});
    assert_eq!(r, quiet);

    // A node restored from a backup made without a passphrase would have the sync records and
    // not the sign-ins. Its records are another machine's (another vault key), so it takes the
    // vault's copies instead of signing the person out everywhere
    let d3 = tempfile::tempdir().unwrap();
    let n3 = node(d3.path());
    {
        let flow = homeserver.start_login("restored.pubky.music").unwrap();
        signer.approve_auth(flow.url()).await.unwrap();
        let session = flow.await_approval().await.unwrap();
        *n3.sync.lock().unwrap() = Some(Arc::new(PubkySync::from_session(
            NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap(),
            session,
        )));
    }
    let a3 = router(n3.clone());
    for k in ["vault.synced.tidal", "vault.synced.jamendo"] {
        let v = get(&n1, k).expect("node 1 has a record");
        set(&n3, k, &v);
    }
    call(&a3, "POST", "/v1/vault/register").await;
    call(&a1, "POST", "/v1/vault/register").await;
    let (_, r) = call(&a3, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pulled"], json!(["tidal", "jamendo"]), "{r}");
    assert_eq!(r["removed"], json!([]), "{r}");
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(r, quiet, "node 1 keeps its sign-ins");

    // Signing out of Jamendo on node 2 removes it from the vault, and node 1 follows; it doesn't
    // come back on node 2's next pass
    n2.library.lock().unwrap().delete_setting("jamendo.user").unwrap();
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["removed"], json!(["jamendo"]), "{r}");
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(r["signed_out"], json!(["jamendo"]), "{r}");
    assert_eq!(get(&n1, "jamendo.user"), None);
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r, quiet);

    // Two nodes that each had their own ListenBrainz before: neither replaces the other
    let (lb1, lb2) = (r#"{"token":"lb-1","user":"a"}"#, r#"{"token":"lb-2","user":"b"}"#);
    set(&n1, "listenbrainz.account", lb1);
    set(&n2, "listenbrainz.account", lb2);
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(r["migrated"], json!(["listenbrainz"]), "{r}");
    call(&a1, "POST", "/v1/vault/register").await;
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["conflicts"], json!(["listenbrainz"]), "{r}");
    // Settings gets why, each side's account (never a token), and what can be taken
    assert_eq!(r["details"][0]["service"], "listenbrainz", "{r}");
    assert_eq!(r["details"][0]["reason"], "other_account");
    assert_eq!(r["details"][0]["node"]["account"], "b");
    assert_eq!(r["details"][0]["vault"]["account"], "a");
    assert_eq!(r["details"][0]["vault"]["written_by"], "node");
    assert_eq!(r["details"][0]["can_take"], json!(["node", "vault"]));
    assert!(
        !r.to_string().contains("lb-1") && !r.to_string().contains("lb-2"),
        "a token in {r}"
    );
    // A take that can't apply is a 409 with the reason; a service the node doesn't keep is a 400
    let (st, v) = post_json(&a2, "/v1/vault/sync", json!({"service": "tidal", "take": "node"})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    let (st, _) = post_json(&a2, "/v1/vault/sync", json!({"service": "plex", "take": "node"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(get(&n2, "listenbrainz.account").as_deref(), Some(lb2));
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(r, quiet, "the vault keeps node 1's");

    // Sign out everywhere from node 1: aside on node 1 now, on node 2 at its next sync (its
    // conflicting copy too)
    // A Spotify sign-in made on node 1 a moment before, not synced yet, goes aside too: it is
    // never pushed back into the wiped vault
    set(
        &n1,
        "spotify.user",
        r#"{"access_token":"sp-1","refresh_token":"sp-r","expires_at":9000,"user_id":"gil"}"#,
    );
    pm_api::vault_cache::new_sign_in(&n1.library.lock().unwrap(), "spotify");
    let (st, r) = call(&a1, "POST", "/v1/vault/sign-out-everywhere").await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["removed"], json!(["listenbrainz", "tidal"]));
    assert_eq!(r["left"], json!([]));
    assert_eq!(r["signed_out_here"], json!(["tidal", "spotify", "listenbrainz"]));
    assert_eq!(get(&n1, "spotify.user"), None);
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["signed_out"], json!(["tidal", "listenbrainz"]), "{r}");
    for n in [&n1, &n2] {
        assert_eq!(get(n, "tidal.user"), None, "the node no longer uses it");
        assert_eq!(get(n, "listenbrainz.account"), None);
        assert_eq!(
            get(n, "vault.signed_out.tidal.user").as_deref(),
            Some(tidal_v2),
            "kept aside"
        );
    }
    // ...and nothing comes back on the next pass
    for a in [&a1, &a2] {
        let (_, r) = call(a, "POST", "/v1/vault/sync").await;
        assert_eq!(r, quiet);
    }
}
