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
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_pubky::homeserver::NodeHomeserver;
use pm_pubky::vault::Vault;
use pm_zones::OutputRegistry;
use pubky::{Keypair, Pubky, PublicKey};
use pubky_music_specs::vault::{ClientSecret, SignIn};
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
    assert!(phone.read("tidal").await.is_err());
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["sign_ins"][0]["epoch"], 2);

    // Sign out everywhere
    let (st, r) = call(&app, "POST", "/v1/vault/sign-out-everywhere").await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["removed"], json!(["tidal"]));
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
    let tidal_v1 = r#"{"access_token":"at-1","refresh_token":"rt-1","user_id":"7","country":"CH"}"#;
    let set = |n: &NodeState, k: &str, v: &str| n.library.lock().unwrap().set_setting(k, v).unwrap();
    let get = |n: &NodeState, k: &str| n.library.lock().unwrap().get_setting(k).unwrap();
    set(&n1, "tidal.user", tidal_v1);
    set(&n1, "youtube.api_key", "AIza-test-key");

    // Node 1: its two sign-ins move into the vault
    let (st, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["migrated"], json!(["tidal", "youtube"]), "{r}");
    // ...and stay where they were on the node
    assert_eq!(get(&n1, "tidal.user").as_deref(), Some(tidal_v1));

    // Node 2 joins: it can't read them until node 1 shares them
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["not_readable"], json!(["tidal", "youtube"]), "{r}");
    call(&a1, "POST", "/v1/vault/sync").await;
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pulled"], json!(["tidal", "youtube"]), "{r}");
    assert_eq!(get(&n2, "tidal.user").as_deref(), Some(tidal_v1));
    assert_eq!(get(&n2, "youtube.api_key").as_deref(), Some("AIza-test-key"));

    // Node 2's source refreshes TIDAL locally: it reaches node 1
    let tidal_v2 = r#"{"access_token":"at-2","refresh_token":"rt-2","user_id":"7","country":"CH"}"#;
    set(&n2, "tidal.user", tidal_v2);
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pushed"], json!(["tidal"]), "{r}");
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pulled"], json!(["tidal"]), "{r}");
    assert_eq!(get(&n1, "tidal.user").as_deref(), Some(tidal_v2));
    // Node 2 refreshes the old account while node 1 signs in to another one: node 1's wins (the
    // person just acted there), and reaches node 2
    let tidal_v3 = r#"{"access_token":"at-3","refresh_token":"rt-3","user_id":"7","country":"CH"}"#;
    set(&n2, "tidal.user", tidal_v3);
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["pushed"], json!(["tidal"]), "{r}");
    let other = r#"{"access_token":"other-1","refresh_token":"other-r","user_id":"9","country":"DE"}"#;
    set(&n1, "tidal.user", other);
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
    assert_eq!(
        r,
        json!({"migrated": [], "pushed": [], "pulled": [], "signed_out": [], "not_readable": []})
    );

    // Sign out everywhere from node 1: aside on node 1 now, on node 2 at its next sync
    let (st, r) = call(&a1, "POST", "/v1/vault/sign-out-everywhere").await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["removed"], json!(["tidal", "youtube"]));
    assert_eq!(r["signed_out_here"], json!(["tidal", "youtube"]));
    let (_, r) = call(&a2, "POST", "/v1/vault/sync").await;
    assert_eq!(r["signed_out"], json!(["tidal", "youtube"]), "{r}");
    for n in [&n1, &n2] {
        assert_eq!(get(n, "tidal.user"), None, "the node no longer uses it");
        assert_eq!(
            get(n, "vault.signed_out.tidal.user").as_deref(),
            Some(tidal_v2),
            "kept aside"
        );
    }
    // ...and nothing comes back on the next pass
    let (_, r) = call(&a1, "POST", "/v1/vault/sync").await;
    assert_eq!(r["migrated"], json!([]), "{r}");
}
